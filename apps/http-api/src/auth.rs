use axum::http::{header, HeaderMap, StatusCode};
pub(crate) use ekyc_domain::product::valid_slug;
use ekyc_postgres::CaseScope;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, env, error::Error, fs};
use subtle::ConstantTimeEq;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigEntry {
    id: String,
    token: String,
    access: Access,
}

#[derive(Deserialize)]
#[serde(tag = "role", rename_all = "snake_case", deny_unknown_fields)]
pub enum Access {
    Product {
        institution: String,
        product: String,
        use_cases: Vec<String>,
    },
    Reviewer {
        institution: String,
        product: String,
        #[serde(default)]
        use_cases: Vec<String>,
    },
    RoutingOperator {
        institution: String,
        products: Vec<String>,
    },
    Observer {
        institution: String,
        products: Vec<String>,
    },
}

fn valid_set(items: &[String]) -> bool {
    !items.is_empty()
        && items.iter().all(|s| valid_slug(s))
        && items.iter().collect::<HashSet<_>>().len() == items.len()
}

impl Access {
    fn valid(&self) -> bool {
        match self {
            Self::Product {
                institution,
                product,
                use_cases,
            } => valid_slug(institution) && valid_slug(product) && valid_set(use_cases),
            Self::Reviewer {
                institution,
                product,
                use_cases,
            } => valid_slug(institution) && valid_slug(product) && use_cases.is_empty(),
            Self::RoutingOperator {
                institution,
                products,
            }
            | Self::Observer {
                institution,
                products,
            } => valid_slug(institution) && valid_set(products),
        }
    }
}

pub struct Caller {
    pub id: String,
    token_hash: [u8; 32],
    pub access: Access,
}

impl Caller {
    pub fn scope(&self) -> CaseScope {
        let (institution, products, owner) = match &self.access {
            Access::Product {
                institution,
                product,
                ..
            } => (institution, vec![product.clone()], Some(self.id.clone())),
            Access::Reviewer {
                institution,
                product,
                ..
            } => (institution, vec![product.clone()], None),
            Access::RoutingOperator {
                institution,
                products,
            }
            | Access::Observer {
                institution,
                products,
            } => (institution, products.clone(), None),
        };
        CaseScope {
            institution: institution.clone(),
            products,
            owner,
        }
    }
    pub fn owns(&self, id: &str) -> bool {
        id.strip_prefix("v2~")
            .and_then(|s| s.split_once('~'))
            .is_some_and(|(owner, digest)| {
                owner == self.id
                    && digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
    }
    pub fn case_id(&self, key: &str) -> String {
        format!("v2~{}~{:x}", self.id, Sha256::digest(key.as_bytes()))
    }
}

pub struct Clients(Vec<std::sync::Arc<Caller>>);

/// Authenticate before parsing the request payload or querying case storage.
pub struct Authenticated(pub std::sync::Arc<Caller>);

impl axum::extract::FromRequestParts<crate::Shared> for Authenticated {
    type Rejection = axum::response::Response;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        app: &crate::Shared,
    ) -> Result<Self, Self::Rejection> {
        app.clients
            .authenticate(&parts.headers)
            .map(|caller| Self(caller.clone()))
            .map_err(|s| crate::error(s, "access_denied"))
    }
}
impl Clients {
    pub fn from_env() -> Result<Self, Box<dyn Error>> {
        let config = match (
            env::var_os("EKYC_CLIENTS"),
            env::var_os("EKYC_CLIENTS_FILE"),
        ) {
            (Some(inline), None) => inline
                .into_string()
                .map_err(|_| "invalid EKYC_CLIENTS encoding")?,
            (None, Some(path)) => fs::read_to_string(path)?,
            _ => return Err("configure exactly one of EKYC_CLIENTS and EKYC_CLIENTS_FILE".into()),
        };
        Ok(Self::parse(&config)?)
    }
    pub fn parse(config: &str) -> Result<Self, &'static str> {
        let entries: Vec<ConfigEntry> =
            serde_json::from_str(config).map_err(|_| "invalid client grant configuration")?;
        let mut ids = HashSet::new();
        let mut hashes = HashSet::new();
        let mut clients = Vec::new();
        for entry in entries {
            if entry.id.is_empty()
                || entry.id.len() > 48
                || !entry
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                || entry.token.len() < 16
                || !entry.token.bytes().all(|b| b.is_ascii_graphic())
                || !entry.access.valid()
            {
                return Err("invalid caller id, token or access grant");
            }
            let token_hash: [u8; 32] = Sha256::digest(entry.token.as_bytes()).into();
            if !ids.insert(entry.id.clone()) || !hashes.insert(token_hash) {
                return Err("duplicate caller or token");
            }
            clients.push(std::sync::Arc::new(Caller {
                id: entry.id,
                token_hash,
                access: entry.access,
            }));
        }
        if clients.is_empty() {
            return Err("at least one caller is required");
        }
        Ok(Self(clients))
    }

    fn authenticate(&self, headers: &HeaderMap) -> Result<&std::sync::Arc<Caller>, StatusCode> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        self.0
            .iter()
            .find(|c| bool::from(c.token_hash.ct_eq(&hash)))
            .ok_or(StatusCode::UNAUTHORIZED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incompatible_grants_and_duplicate_tokens_fail_closed() {
        let sample = |access: &str| {
            format!(r#"[{{"id":"app","token":"a-sufficiently-long-token","access":{access}}}]"#)
        };
        for grant in [
            r#"{"role":"observer","institution":"bank","products":[]}"#,
            r#"{"role":"observer","institution":"bank","products":["wallet","wallet"]}"#,
            r#"{"role":"reviewer","institution":"bank","product":"wallet","use_cases":["onboarding"]}"#,
            r#"{"role":"product","institution":"bank","product":"wallet","use_cases":[]}"#,
        ] {
            assert!(Clients::parse(&sample(grant)).is_err());
        }
        let input = sample(
            r#"{"role":"product","institution":"bank","product":"wallet","use_cases":["onboarding"]}"#,
        );
        let clients = Clients::parse(&input).unwrap();
        let caller = &clients.0[0];
        assert!(caller.owns(&caller.case_id("private")));
        assert!(!caller.owns(&caller.case_id("private").replace("app~", "other~")));
        assert!(!caller.owns(&format!("{}~more", caller.case_id("private"))));
        assert!(Clients::parse(&format!(
            "[{},{}]",
            &input[1..input.len() - 1],
            input[1..input.len() - 1].replace("app", "another")
        ))
        .is_err());
    }
}

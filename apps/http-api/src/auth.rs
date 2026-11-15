use axum::http::{header, HeaderMap};
use ekyc_domain::Purpose;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use subtle::ConstantTimeEq;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigEntry {
    id: String,
    token: String,
    purposes: Vec<Purpose>,
}

pub struct Caller {
    pub id: String,
    token_hash: [u8; 32],
    pub purposes: Vec<Purpose>,
}

pub struct Clients(Vec<Caller>);

impl Clients {
    pub fn parse(config: &str) -> Result<Self, &'static str> {
        let entries: Vec<ConfigEntry> =
            serde_json::from_str(config).map_err(|_| "invalid EKYC_CLIENTS JSON")?;
        let mut ids = HashSet::new();
        let mut hashes = HashSet::new();
        let mut clients = Vec::new();
        for entry in entries {
            if entry.id.is_empty()
                || entry.id.len() > 48
                || !entry
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
                || entry.token.len() < 16
                || !entry.token.bytes().all(|c| c.is_ascii_graphic())
                || entry.purposes.is_empty()
            {
                return Err("invalid caller id, token, or purposes");
            }
            let token_hash: [u8; 32] = Sha256::digest(entry.token.as_bytes()).into();
            if !ids.insert(entry.id.clone()) || !hashes.insert(token_hash) {
                return Err("duplicate caller or token");
            }
            clients.push(Caller {
                id: entry.id,
                token_hash,
                purposes: entry.purposes,
            });
        }
        if clients.is_empty() {
            return Err("at least one caller is required");
        }
        Ok(Self(clients))
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Option<&Caller> {
        let token = headers
            .get(header::AUTHORIZATION)?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?;
        let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        self.0
            .iter()
            .find(|caller| bool::from(caller.token_hash.ct_eq(&hash)))
    }
}

// IDs are scoped to one authenticated caller. Valid IDs cannot contain the separator.
pub fn case_id(caller: &str, key: &str) -> String {
    format!("{}~{:x}", caller, Sha256::digest(key.as_bytes()))
}

pub fn owned_by(id: &str, caller: &str) -> bool {
    id.split_once('~').is_some_and(|(prefix, hash)| {
        prefix == caller
            && hash.len() == 64
            && hash
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn namespaces_do_not_overlap() {
        let first = case_id("a", "same");
        assert!(owned_by(&first, "a"));
        assert!(!owned_by(&first, "a-1"));
        assert!(!owned_by(&format!("{first}~other"), "a"));
        assert_ne!(first, case_id("a-1", "same"));
    }
}

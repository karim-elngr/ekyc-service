//! Version 2 HTTP command bodies and durable command receipts.
use super::ReviewAction;
use crate::CheckKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Open {
    pub subject_ref: String,
    pub use_case: String,
    #[serde(default)]
    pub evidence: BTreeMap<CheckKind, String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub evidence: BTreeMap<CheckKind, String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Retry {
    pub check: CheckKind,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub operation_id: String,
    pub check: CheckKind,
    pub disposition: ReviewAction,
    pub reason: String,
    pub review_ref: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Open(Open),
    Evidence(Evidence),
    Retry(Retry),
    Review(Review),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub institution: String,
    pub caller: String,
    pub product: String,
    pub key: String,
    pub action: Action,
}

impl Command {
    pub fn receipt_key(&self) -> String {
        format!("command/{}/{}", self.caller, self.key)
    }
}

/// A reviewer may enter up to 512 Unicode scalar values and use line breaks.
pub fn valid_review_reason(reason: &str) -> bool {
    !reason.trim().is_empty()
        && reason.chars().count() <= 512
        && reason.chars().all(|ch| ch == '\n' || !ch.is_control())
}

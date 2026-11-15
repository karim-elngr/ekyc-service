//! Evidence checks shared by product policies and provider adapters.

pub mod product;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Document,
    Face,
    Address,
    Sanctions,
}

impl CheckKind {
    pub const ALL: [Self; 4] = [Self::Document, Self::Face, Self::Address, Self::Sanctions];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Face => "face",
            Self::Address => "address",
            Self::Sanctions => "sanctions",
        }
    }
}

/// Opaque identifiers are nonblank, control-free and bounded in UTF-8 bytes.
pub fn valid_reference(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Observation {
    Document {
        passed: bool,
        confidence: f64,
        expires_on: NaiveDate,
    },
    Face {
        passed: bool,
        confidence: f64,
        similarity: f64,
    },
    Address {
        passed: bool,
        confidence: f64,
        proof_date: NaiveDate,
    },
    Sanctions {
        hit: bool,
    },
    Unavailable,
}

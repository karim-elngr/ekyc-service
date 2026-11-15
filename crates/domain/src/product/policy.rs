use super::{Assessment, BindingSnapshot, CheckStatus, Fact};
use crate::{valid_reference, CheckKind, Observation};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The complete validated policy is pinned in the case for deterministic replay.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub product: String,
    pub use_case: String,
    pub version: String,
    pub checks: Vec<Rule>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub kind: CheckKind,
    pub methods: Vec<String>,
    pub min_confidence: Option<f64>,
    pub min_similarity: Option<f64>,
    pub max_proof_age_days: Option<u32>,
    pub reuse: Option<Reuse>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reuse {
    pub source_products: Vec<String>,
    pub max_age_days: u32,
}

/// Product and use-case identifiers share the grammar accepted in client grants.
pub fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

impl Policy {
    pub fn load(product: &str, use_case: &str) -> Result<Self, String> {
        for definition in [
            include_str!("../../policies/wallet-onboarding-v1.json"),
            include_str!("../../policies/wallet-address-refresh-v1.json"),
            include_str!("../../policies/wallet-full-onboarding-v1.json"),
            include_str!("../../policies/lending-loan-application-v1.json"),
        ] {
            let policy = Self::from_json(definition)?;
            if policy.product == product && policy.use_case == use_case {
                return Ok(policy);
            }
        }
        Err("unknown product and use case".into())
    }

    pub fn from_json(definition: &str) -> Result<Self, String> {
        let policy: Self = serde_json::from_str(definition).map_err(|e| e.to_string())?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<(), String> {
        let nonempty_unique = |values: &[String]| {
            !values.is_empty()
                && values.iter().all(|v| valid_reference(v))
                && values.iter().collect::<BTreeSet<_>>().len() == values.len()
        };
        if !valid_slug(&self.product)
            || !valid_slug(&self.use_case)
            || !valid_reference(&self.version)
            || self.checks.is_empty()
        {
            return Err("invalid policy identity or empty check plan".into());
        }
        let mut kinds = BTreeSet::new();
        for rule in &self.checks {
            let is_face = rule.kind == CheckKind::Face;
            let is_address = rule.kind == CheckKind::Address;
            let is_screen = rule.kind == CheckKind::Sanctions;
            if !kinds.insert(rule.kind)
                || !nonempty_unique(&rule.methods)
                || rule.min_confidence.is_some() == is_screen
                || rule.min_similarity.is_some() != is_face
                || rule.max_proof_age_days.is_some() != is_address
                || [rule.min_confidence, rule.min_similarity]
                    .into_iter()
                    .flatten()
                    .any(|n| !(0.0..=100.0).contains(&n))
            {
                return Err(format!("invalid rule for {}", rule.kind.as_str()));
            }
            if let Some(reuse) = &rule.reuse {
                if !matches!(rule.kind, CheckKind::Document | CheckKind::Face)
                    || !nonempty_unique(&reuse.source_products)
                    || !reuse.source_products.iter().all(|s| valid_slug(s))
                    || reuse.max_age_days == 0
                {
                    return Err("unsupported reuse rule or missing sources/freshness window".into());
                }
            }
        }
        if kinds.contains(&CheckKind::Sanctions) && !kinds.contains(&CheckKind::Document) {
            return Err("sanctions requires a verified document identity".into());
        }
        Ok(())
    }
}

impl Rule {
    pub fn accepts_source(&self, fact: &Fact, now: DateTime<Utc>) -> bool {
        fact.received_at.is_some()
            && fact.source_passed
            && self.assess(&fact.observation, now).status == CheckStatus::Passed
    }

    pub fn matches_candidate(
        &self,
        fact: &Fact,
        case_id: &str,
        tenant: &str,
        subject: &str,
        bindings: &BindingSnapshot,
        now: DateTime<Utc>,
    ) -> bool {
        (!fact.source_passed || fact.received_at.is_some())
            && fact.origin_case_id != case_id
            && valid_reference(&fact.origin_case_id)
            && valid_reference(&fact.operation_id)
            && fact.provider == fact.evidence.provider
            && self.reuse_allows(fact, now)
            && fact.evidence.matches(tenant, subject, self, bindings)
    }

    pub(super) fn latest_candidate<'a>(
        &self,
        candidates: &'a [Fact],
        case_id: &str,
        tenant: &str,
        subject: &str,
        bindings: &BindingSnapshot,
        now: DateTime<Utc>,
    ) -> Option<&'a Fact> {
        candidates
            .iter()
            .filter(|fact| self.matches_candidate(fact, case_id, tenant, subject, bindings, now))
            .max_by(|a, b| self.compare_candidates(a, b, now))
    }

    pub fn compare_candidates(&self, a: &Fact, b: &Fact, now: DateTime<Utc>) -> std::cmp::Ordering {
        a.observed_at
            .cmp(&b.observed_at)
            // At the same observation time adverse findings take precedence.
            .then_with(|| {
                self.accepts_source(b, now)
                    .cmp(&self.accepts_source(a, now))
            })
            .then_with(|| a.operation_id.cmp(&b.operation_id))
    }

    pub(super) fn reuse_allows(&self, fact: &Fact, now: DateTime<Utc>) -> bool {
        self.reuse.as_ref().is_some_and(|reuse| {
            reuse.source_products.contains(&fact.source_product)
                && fact.observed_at <= now
                && now.signed_duration_since(fact.observed_at)
                    <= Duration::days(i64::from(reuse.max_age_days))
        })
    }

    pub(super) fn assess(&self, observation: &Observation, now: DateTime<Utc>) -> Assessment {
        use CheckStatus::*;
        let mut reasons = Vec::new();
        let status = match (self.kind, observation) {
            (
                CheckKind::Document,
                Observation::Document {
                    passed,
                    confidence,
                    expires_on,
                },
            ) => {
                if *expires_on < now.date_naive() {
                    reasons.push("DOCUMENT_EXPIRED");
                }
                if !passed {
                    reasons.push("DOCUMENT_NOT_VERIFIED");
                }
                score(
                    &mut reasons,
                    *confidence,
                    self.min_confidence,
                    "DOCUMENT_CONFIDENCE_LOW",
                );
                AwaitingEvidence
            }
            (
                CheckKind::Face,
                Observation::Face {
                    passed,
                    confidence,
                    similarity,
                },
            ) => {
                if !passed {
                    reasons.push("FACE_NOT_VERIFIED");
                }
                score(
                    &mut reasons,
                    *confidence,
                    self.min_confidence,
                    "FACE_CONFIDENCE_LOW",
                );
                score(
                    &mut reasons,
                    *similarity,
                    self.min_similarity,
                    "FACE_SIMILARITY_LOW",
                );
                AwaitingEvidence
            }
            (
                CheckKind::Address,
                Observation::Address {
                    passed,
                    confidence,
                    proof_date,
                },
            ) => {
                let age = now
                    .date_naive()
                    .signed_duration_since(*proof_date)
                    .num_days();
                if age < 0 {
                    reasons.push("ADDRESS_PROOF_FUTURE");
                }
                if age > i64::from(self.max_proof_age_days.unwrap_or(0)) {
                    reasons.push("ADDRESS_PROOF_STALE");
                }
                if !passed {
                    reasons.push("ADDRESS_NOT_VERIFIED");
                }
                score(
                    &mut reasons,
                    *confidence,
                    self.min_confidence,
                    "ADDRESS_CONFIDENCE_LOW",
                );
                AwaitingEvidence
            }
            (CheckKind::Sanctions, Observation::Sanctions { hit }) => {
                if *hit {
                    reasons.push("SANCTIONS_POTENTIAL_MATCH");
                }
                Review
            }
            (_, Observation::Unavailable) => {
                reasons.push(if self.kind == CheckKind::Sanctions {
                    "SANCTIONS_UNAVAILABLE"
                } else {
                    "CHECK_UNAVAILABLE"
                });
                Blocked
            }
            _ => {
                reasons.push("INVALID_OBSERVATION");
                Blocked
            }
        };
        let status = if reasons.is_empty() {
            Passed
        } else if reasons.contains(&"INVALID_SCORE") {
            Blocked
        } else {
            status
        };
        Assessment {
            status,
            reasons: reasons.into_iter().map(str::to_owned).collect(),
        }
    }
}

fn score(reasons: &mut Vec<&'static str>, value: f64, minimum: Option<f64>, low: &'static str) {
    if !(0.0..=100.0).contains(&value) {
        reasons.push("INVALID_SCORE");
    } else if value <= minimum.unwrap_or(100.0) {
        reasons.push(low);
    }
}

//! Verification vocabulary and deterministic evaluation of the v1 policy.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const POLICY_VERSION: &str = "v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Document,
    Face,
    Address,
    Sanctions,
}

impl CheckKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Face => "face",
            Self::Address => "address",
            Self::Sanctions => "sanctions",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    FullOnboarding,
    AddressRefresh,
}

impl Purpose {
    pub fn checks(self) -> &'static [CheckKind] {
        match self {
            Self::FullOnboarding => &[
                CheckKind::Document,
                CheckKind::Face,
                CheckKind::Address,
                CheckKind::Sanctions,
            ],
            Self::AddressRefresh => &[CheckKind::Address],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenCase {
    pub subject_ref: String,
    pub purpose: Purpose,
    #[serde(default)]
    pub evidence: BTreeMap<CheckKind, String>,
}

impl OpenCase {
    pub fn validate(&self) -> Result<(), String> {
        fn valid_ref(value: &str) -> bool {
            !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
        }

        if !valid_ref(&self.subject_ref) {
            return Err("invalid subject_ref".into());
        }
        for (kind, reference) in &self.evidence {
            if !self.purpose.checks().contains(kind) {
                return Err(format!(
                    "{} is outside the requested purpose",
                    kind.as_str()
                ));
            }
            if !valid_ref(reference) {
                return Err(format!("invalid {} evidence reference", kind.as_str()));
            }
        }
        Ok(())
    }
}

/// A missing map entry means a check is still pending. An explicit Missing
/// records an admitted case with no evidence for the check.
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
    Missing,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    Approved,
    ManualReview,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    pub check: CheckKind,
    pub code: String,
}

impl Reason {
    /// Messages are safe for the authorized caller and do not include vendor payloads.
    pub fn public_message(&self) -> &'static str {
        match self.code.as_str() {
            "DOCUMENT_EXPIRED" => "The identity document had expired on the evaluation date.",
            "DOCUMENT_NOT_VERIFIED" => "The provider could not verify the identity document.",
            "DOCUMENT_CONFIDENCE_LOW" => {
                "Document confidence did not exceed the required threshold."
            }
            "FACE_NOT_VERIFIED" => "The provider could not verify the face match.",
            "FACE_CONFIDENCE_LOW" => "Face-match confidence did not exceed the required threshold.",
            "FACE_SIMILARITY_LOW" => "Face similarity did not exceed the required threshold.",
            "ADDRESS_NOT_VERIFIED" => "The provider could not verify the address.",
            "ADDRESS_PROOF_FUTURE" => "The address proof was dated after the evaluation date.",
            "ADDRESS_PROOF_STALE" => "The address proof was older than 90 days.",
            "ADDRESS_CONFIDENCE_LOW" => "Address confidence did not exceed the required threshold.",
            "SANCTIONS_HIT" => "The sanctions provider reported a match.",
            "SANCTIONS_UNAVAILABLE" => "Sanctions screening could not be completed.",
            "CHECK_UNAVAILABLE" => "The verification check could not be completed.",
            "MISSING_EVIDENCE" => "Required evidence was missing.",
            "INVALID_OBSERVATION" => "The provider result could not be interpreted for this check.",
            "INVALID_SCORE" => "A provider score was outside its permitted range.",
            _ => "The recorded policy returned a verification finding.",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckResult {
    Pending,
    Observed,
    Pass,
    Review,
    Fail,
    SkippedAfterDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSummary {
    pub kind: CheckKind,
    pub result: CheckResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub verdict: Verdict,
    pub reasons: Vec<Reason>,
}

impl Decision {
    fn record(&mut self, check: CheckKind, code: &str, verdict: Verdict) {
        if self.verdict == Verdict::Approved || verdict == Verdict::Rejected {
            self.verdict = verdict;
        }
        self.reasons.push(Reason {
            check,
            code: code.into(),
        });
    }
}

/// Evaluate a pinned plan against the selected facts at a journaled UTC date.
/// A hard rejection can close an incomplete case. Review and approval require
/// all scoped checks. Unrelated facts never change the result.
pub fn evaluate(
    facts: &BTreeMap<CheckKind, Observation>,
    plan: &[CheckKind],
    as_of: NaiveDate,
) -> Option<Decision> {
    if plan.is_empty() {
        return None;
    }

    let mut complete = true;
    let mut decision = Decision {
        verdict: Verdict::Approved,
        reasons: Vec::new(),
    };
    for &check in plan {
        match facts.get(&check) {
            Some(observation) => assess(check, observation, as_of, &mut decision),
            None => complete = false,
        }
    }
    (complete || decision.verdict == Verdict::Rejected).then_some(decision)
}

/// Build a read-model revision as part of the write. Open cases expose progress;
/// terminal cases use the same pinned UTC evaluation date as their decision.
pub fn summarize_checks(
    facts: &BTreeMap<CheckKind, Observation>,
    plan: &[CheckKind],
    decided_on: Option<NaiveDate>,
) -> Vec<CheckSummary> {
    plan.iter()
        .map(|&kind| {
            let result = match (facts.get(&kind), decided_on) {
                (None, None) => CheckResult::Pending,
                (Some(_), None) => CheckResult::Observed,
                (None, Some(_)) => CheckResult::SkippedAfterDecision,
                (Some(fact), Some(date)) => {
                    let mut decision = Decision {
                        verdict: Verdict::Approved,
                        reasons: Vec::new(),
                    };
                    assess(kind, fact, date, &mut decision);
                    match decision.verdict {
                        Verdict::Approved => CheckResult::Pass,
                        Verdict::ManualReview => CheckResult::Review,
                        Verdict::Rejected => CheckResult::Fail,
                    }
                }
            };
            CheckSummary { kind, result }
        })
        .collect()
}

fn assess(check: CheckKind, observation: &Observation, as_of: NaiveDate, out: &mut Decision) {
    use Verdict::{ManualReview, Rejected};

    let inconclusive = if check == CheckKind::Sanctions {
        Rejected
    } else {
        ManualReview
    };

    match (check, observation) {
        (
            CheckKind::Document,
            Observation::Document {
                passed,
                confidence,
                expires_on,
            },
        ) => {
            if *expires_on < as_of {
                out.record(check, "DOCUMENT_EXPIRED", Rejected);
            }
            if !passed {
                out.record(check, "DOCUMENT_NOT_VERIFIED", ManualReview);
            }
            score(out, check, *confidence, 85.0, "DOCUMENT_CONFIDENCE_LOW");
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
                out.record(check, "FACE_NOT_VERIFIED", ManualReview);
            }
            score(out, check, *confidence, 85.0, "FACE_CONFIDENCE_LOW");
            score(out, check, *similarity, 85.0, "FACE_SIMILARITY_LOW");
        }
        (
            CheckKind::Address,
            Observation::Address {
                passed,
                confidence,
                proof_date,
            },
        ) => {
            if !passed {
                out.record(check, "ADDRESS_NOT_VERIFIED", ManualReview);
            }
            let age = as_of.signed_duration_since(*proof_date).num_days();
            if age < 0 {
                out.record(check, "ADDRESS_PROOF_FUTURE", ManualReview);
            } else if age > 90 {
                out.record(check, "ADDRESS_PROOF_STALE", ManualReview);
            }
            score(out, check, *confidence, 80.0, "ADDRESS_CONFIDENCE_LOW");
        }
        (CheckKind::Sanctions, Observation::Sanctions { hit }) => {
            if *hit {
                out.record(check, "SANCTIONS_HIT", Rejected);
            }
        }
        (_, Observation::Missing) => out.record(check, "MISSING_EVIDENCE", inconclusive),
        (CheckKind::Sanctions, Observation::Unavailable) => {
            out.record(check, "SANCTIONS_UNAVAILABLE", Rejected);
        }
        (_, Observation::Unavailable) => out.record(check, "CHECK_UNAVAILABLE", ManualReview),
        _ => out.record(check, "INVALID_OBSERVATION", inconclusive),
    }
}

fn score(out: &mut Decision, check: CheckKind, value: f64, threshold: f64, low: &str) {
    if !(0.0..=100.0).contains(&value) {
        out.record(check, "INVALID_SCORE", Verdict::ManualReview);
    } else if value <= threshold {
        out.record(check, low, Verdict::ManualReview);
    }
}

#[cfg(test)]
mod tests;

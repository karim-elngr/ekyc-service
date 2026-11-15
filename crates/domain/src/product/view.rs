use super::{CaseState, CheckStatus, Origin, ProductCase};
use crate::CheckKind;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Version 2 read projection, shared by case history, point reads and webhook publication.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublicCase {
    pub case_id: String,
    pub product: String,
    pub use_case: String,
    pub subject_ref: String,
    pub policy_version: String,
    pub state: CaseState,
    pub version: i64,
    pub checks: Vec<PublicCheck>,
    pub reasons: Vec<PublicReason>,
    pub next_action: Option<String>,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublicCheck {
    pub kind: CheckKind,
    pub status: CheckStatus,
    pub origin: Option<Origin>,
    pub origin_case_id: Option<String>,
    pub operation_id: Option<String>,
    pub observed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub received_at: Option<DateTime<Utc>>,
    pub interactive: bool,
    pub attempt: u32,
    pub reasons: Vec<String>,
    pub next_action: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublicReason {
    pub check: CheckKind,
    pub code: String,
}

impl ProductCase {
    pub fn public_view(&self) -> PublicCase {
        let checks = self
            .policy
            .checks
            .iter()
            .map(|rule| {
                let check = &self.checks[&rule.kind];
                let origin_case_id = check
                    .fact
                    .as_ref()
                    .map(|fact| fact.origin_case_id.clone())
                    .or_else(|| {
                        (check.origin == Some(Origin::Fresh)).then(|| self.case_id.clone())
                    });
                PublicCheck {
                    kind: check.kind,
                    status: check.status,
                    origin: check.origin,
                    origin_case_id,
                    operation_id: check.operation_id.clone(),
                    observed_at: check.fact.as_ref().map(|fact| fact.observed_at),
                    received_at: check.fact.as_ref().and_then(|fact| fact.received_at),
                    interactive: check.is_interactive(),
                    attempt: check.attempt,
                    reasons: check.reasons.clone(),
                    next_action: if self.is_terminal() {
                        None
                    } else {
                        check.next_action().map(str::to_owned)
                    },
                }
            })
            .collect();
        let mut reasons: Vec<_> = self
            .checks
            .values()
            .flat_map(|check| {
                check.reasons.iter().map(move |code| PublicReason {
                    check: check.kind,
                    code: code.clone(),
                })
            })
            .collect();
        let waiting = self
            .screen_obligations
            .values()
            .any(|h| h.outcome.is_none());
        let investigate = self
            .screen_obligations
            .values()
            .any(super::ScreenObligation::awaiting_disposition);
        if waiting || investigate {
            reasons.push(PublicReason {
                check: CheckKind::Sanctions,
                code: if investigate {
                    "ORIGINAL_SCREENING_REVIEW"
                } else {
                    "ORIGINAL_SCREENING_OUTSTANDING"
                }
                .into(),
            });
        }
        let next_action = match self.state {
            CaseState::Running => Some("wait_for_provider"),
            CaseState::AwaitingEvidence => Some("provide_evidence"),
            CaseState::ProviderBlocked if waiting => Some("await_original_screening"),
            CaseState::ProviderBlocked
                if self.routed_address()
                    && self
                        .checks
                        .get(&CheckKind::Address)
                        .is_some_and(|s| s.status == CheckStatus::Blocked) =>
            {
                Some("contact_provider_operations")
            }
            CaseState::ProviderBlocked => Some("retry"),
            CaseState::AwaitingReview => Some("review"),
            CaseState::Approved | CaseState::Rejected => None,
        };
        PublicCase {
            case_id: self.case_id.clone(),
            product: self.policy.product.clone(),
            use_case: self.policy.use_case.clone(),
            subject_ref: self.subject_ref.clone(),
            policy_version: self.policy.version.clone(),
            state: self.state,
            version: self.revision,
            checks,
            reasons,
            next_action: next_action.map(str::to_owned),
            created_at: self.created_at,
            decided_at: self.decided_at,
        }
    }

    pub fn view(&self) -> Value {
        serde_json::to_value(self.public_view()).expect("public case fields serialize as JSON")
    }
}

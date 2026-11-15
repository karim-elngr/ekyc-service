use super::{BoundEvidence, Fact};
use crate::CheckKind;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CaseState {
    Running,
    AwaitingEvidence,
    ProviderBlocked,
    AwaitingReview,
    Approved,
    Rejected,
}

impl CaseState {
    pub const ALL: [Self; 6] = [
        Self::Running,
        Self::AwaitingEvidence,
        Self::ProviderBlocked,
        Self::AwaitingReview,
        Self::Approved,
        Self::Rejected,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "RUNNING",
            Self::AwaitingEvidence => "AWAITING_EVIDENCE",
            Self::ProviderBlocked => "PROVIDER_BLOCKED",
            Self::AwaitingReview => "AWAITING_REVIEW",
            Self::Approved => "APPROVED",
            Self::Rejected => "REJECTED",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckStatus {
    AwaitingEvidence,
    Pending,
    Passed,
    Blocked,
    Review,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Fresh,
    Reused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAction {
    Clear,
    Confirm,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRecord {
    pub kind: CheckKind,
    pub operation_id: String,
    pub action: ReviewAction,
    pub actor: String,
    pub reason: String,
    pub review_ref: String,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProductCheck {
    pub kind: CheckKind,
    pub status: CheckStatus,
    pub origin: Option<Origin>,
    pub evidence: Option<BoundEvidence>,
    pub operation_id: Option<String>,
    pub attempt: u32,
    pub fact: Option<Fact>,
    pub review: Option<ReviewRecord>,
    pub reasons: Vec<String>,
}

impl ProductCheck {
    /// The callback belongs to this fresh operation, never to a reused historical fact.
    pub fn is_interactive(&self) -> bool {
        self.origin == Some(Origin::Fresh) && self.evidence.as_ref().is_some_and(|e| e.callback)
    }

    pub(super) fn empty(kind: CheckKind) -> Self {
        Self {
            kind,
            status: CheckStatus::AwaitingEvidence,
            origin: None,
            evidence: None,
            operation_id: None,
            attempt: 0,
            fact: None,
            review: None,
            reasons: vec!["MISSING_EVIDENCE".into()],
        }
    }

    pub(super) fn clear(&mut self, reason: &str) {
        let attempt = self.attempt;
        *self = Self::empty(self.kind);
        self.attempt = attempt;
        self.reasons = vec![reason.into()];
    }

    pub(super) fn start(&mut self, case_id: &str, evidence: BoundEvidence) {
        self.attempt += 1;
        self.operation_id = Some(format!("{case_id}/{}/{}", self.kind.as_str(), self.attempt));
        self.origin = Some(Origin::Fresh);
        self.evidence = Some(evidence);
        self.fact = None;
        self.review = None;
        self.reasons.clear();
        self.status = CheckStatus::Pending;
    }

    pub(super) fn next_action(&self) -> Option<&'static str> {
        match (self.kind, self.status) {
            (CheckKind::Sanctions, CheckStatus::AwaitingEvidence) => Some("verify_document"),
            (_, CheckStatus::AwaitingEvidence) => Some("provide_evidence"),
            (_, CheckStatus::Pending) => Some("wait_for_provider"),
            (_, CheckStatus::Blocked) => Some("retry"),
            (_, CheckStatus::Review) => Some("review"),
            _ => None,
        }
    }
}

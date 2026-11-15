//! Pinned screening obligations survive replacement, evidence refresh and provider timeouts.
use super::{BoundEvidence, CaseState, CheckStatus, ProductCase, ReviewAction, ReviewRecord};
use crate::{valid_reference, CheckKind, Observation};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenOutcome {
    Clear,
    Hit,
    NotAccepted,
    Inconclusive,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScreenObligation {
    pub original_operation: String,
    pub original_evidence: BoundEvidence,
    pub replacement_operation: Option<String>,
    pub armed_at: DateTime<Utc>,
    pub assign_at: DateTime<Utc>,
    pub assigned_to: Option<String>,
    pub outcome: Option<ScreenOutcome>,
    pub observed_at: Option<DateTime<Utc>>,
    pub received_at: Option<DateTime<Utc>>,
    pub review: Option<ReviewRecord>,
}

impl ScreenObligation {
    pub fn awaiting_disposition(&self) -> bool {
        self.review.is_none()
            && matches!(
                self.outcome,
                Some(ScreenOutcome::Hit | ScreenOutcome::Inconclusive)
            )
    }
}

impl ProductCase {
    pub fn strict_screening(&self) -> bool {
        self.policy.strict_screen.is_some()
    }

    pub(super) fn screening_candidate(&self, provider: &str) -> Option<BoundEvidence> {
        let current = self.bindings.screening.as_ref()?;
        let rule = self
            .policy
            .checks
            .iter()
            .find(|r| r.kind == CheckKind::Sanctions)?;
        self.bindings
            .provider_candidates
            .get(&CheckKind::Sanctions)?
            .iter()
            .find(|asset| {
                asset.provider == provider
                    && asset.matches(&self.tenant_id, &self.subject_ref, rule, &self.bindings)
                    && asset.evidence_ref == current.evidence_ref
                    && asset.revision == current.revision
                    && asset.document_revision == current.document_revision
                    && !asset.callback
            })
            .cloned()
    }

    pub(super) fn arm_screen(&mut self, now: DateTime<Utc>) {
        let Some(policy) = &self.policy.strict_screen else {
            return;
        };
        let screen = &self.checks[&CheckKind::Sanctions];
        let Some((operation, asset)) = screen.operation_id.as_ref().zip(screen.evidence.as_ref())
        else {
            return;
        };
        let linked_replacement = asset.provider == policy.replacement
            && self
                .screen_obligations
                .values()
                .any(|h| h.replacement_operation.as_ref() == Some(operation));
        if screen.status == CheckStatus::Pending
            && (asset.provider == policy.primary || linked_replacement)
        {
            self.screen_obligations
                .entry(operation.clone())
                .or_insert_with(|| ScreenObligation {
                    original_operation: operation.clone(),
                    original_evidence: asset.clone(),
                    replacement_operation: None,
                    armed_at: now,
                    assign_at: now + Duration::milliseconds(policy.assign_after_ms.into()),
                    assigned_to: None,
                    outcome: None,
                    observed_at: None,
                    received_at: None,
                    review: None,
                });
        }
    }

    pub(super) fn replace_screen(&mut self, operation: &str) -> bool {
        let Some(policy) = &self.policy.strict_screen else {
            return false;
        };
        let Some(hold) = self.screen_obligations.get(operation) else {
            return false;
        };
        if hold.original_evidence.provider != policy.primary
            || hold.replacement_operation.is_some()
            || matches!(
                hold.outcome,
                Some(ScreenOutcome::Clear | ScreenOutcome::Hit)
            )
        {
            return false;
        }
        let Some(binding) = self.screening_candidate(&policy.replacement) else {
            return false;
        };
        if binding.evidence_ref != hold.original_evidence.evidence_ref
            || binding.revision != hold.original_evidence.revision
        {
            return false;
        }
        let screen = self
            .checks
            .get_mut(&CheckKind::Sanctions)
            .expect("strict policy requires screening");
        if screen.operation_id.as_deref() != Some(operation) {
            return false;
        }
        screen.start(&self.case_id, binding);
        self.screen_obligations
            .get_mut(operation)
            .unwrap()
            .replacement_operation
            .clone_from(&screen.operation_id);
        true
    }

    /// Reapply the obligations at the product decision boundary, including SQL publication.
    pub fn guard_screening_decision(&mut self) {
        let holds = self.screen_obligations.values();
        let verified_screen = self.checks.get(&CheckKind::Sanctions).is_some_and(|check| {
            check.status == CheckStatus::Passed
                && check.operation_id.as_ref().is_some_and(|operation| {
                    holds.clone().any(|hold| {
                        let same_identity = check.evidence.as_ref().is_some_and(|asset| {
                            asset.evidence_ref == hold.original_evidence.evidence_ref
                                && asset.revision == hold.original_evidence.revision
                        });
                        same_identity
                            && if hold.original_operation == *operation {
                                hold.outcome == Some(ScreenOutcome::Clear)
                                    || hold
                                        .review
                                        .as_ref()
                                        .is_some_and(|r| r.action == ReviewAction::Clear)
                            } else {
                                hold.replacement_operation.as_ref() == Some(operation)
                            }
                    })
                })
        });
        if holds.clone().any(|h| {
            h.review
                .as_ref()
                .is_some_and(|r| r.action == ReviewAction::Confirm)
        }) {
            self.state = CaseState::Rejected;
        } else if self.state != CaseState::Rejected {
            if holds.clone().any(ScreenObligation::awaiting_disposition) {
                self.state = CaseState::AwaitingReview;
            } else if self.state == CaseState::Approved
                && self.strict_screening()
                && (!verified_screen || holds.clone().any(|h| h.outcome.is_none()))
            {
                self.state = CaseState::ProviderBlocked;
            }
        }
        if !self.is_terminal() {
            self.decided_at = None;
        }
    }

    pub fn assign_original(&mut self, operation: &str, now: DateTime<Utc>) -> bool {
        let Some(policy) = &self.policy.strict_screen else {
            return false;
        };
        let Some(hold) = self.screen_obligations.get_mut(operation) else {
            return false;
        };
        if hold.assigned_to.is_some() || hold.outcome.is_some() || now < hold.assign_at {
            return false;
        }
        hold.assigned_to = Some(policy.owner.clone());
        self.revision += 1;
        true
    }

    /// Result-only lookups can settle the original after the submission deadline.
    pub fn record_screen_lookup(
        &mut self,
        operation: &str,
        outcome: ScreenOutcome,
        observed_at: DateTime<Utc>,
        received_at: DateTime<Utc>,
        decision_at: DateTime<Utc>,
    ) -> Result<(), String> {
        let Some(hold) = self.screen_obligations.get_mut(operation) else {
            return Ok(());
        };
        if hold.outcome.is_some() || (observed_at > received_at && outcome == ScreenOutcome::Clear)
        {
            return Ok(());
        }
        hold.outcome = Some(outcome);
        hold.observed_at = Some(observed_at);
        hold.received_at = Some(received_at);
        if matches!(outcome, ScreenOutcome::Hit | ScreenOutcome::Inconclusive) {
            hold.assigned_to
                .get_or_insert_with(|| self.policy.strict_screen.as_ref().unwrap().owner.clone());
        }
        // If B definitively never accepted, A's own authoritative result can satisfy
        // this revision. Neither a missed response nor an inconclusive B can do so.
        if !self.is_terminal() && self.resume_after_unaccepted_replacement(decision_at)? {
            return Ok(());
        }
        let screen = &self.checks[&CheckKind::Sanctions];
        if screen.operation_id.as_deref() == Some(operation) && !self.is_terminal() {
            match outcome {
                ScreenOutcome::Clear | ScreenOutcome::Hit => {
                    self.checks.get_mut(&CheckKind::Sanctions).unwrap().status =
                        CheckStatus::Pending;
                    self.record_received(
                        CheckKind::Sanctions,
                        operation,
                        Observation::Sanctions {
                            hit: outcome == ScreenOutcome::Hit,
                        },
                        observed_at,
                        Some(received_at),
                        decision_at,
                    )?;
                    return Ok(());
                }
                ScreenOutcome::NotAccepted | ScreenOutcome::Inconclusive => {
                    if !self.replace_screen(operation) {
                        let screen = self.checks.get_mut(&CheckKind::Sanctions).unwrap();
                        screen.status = CheckStatus::Blocked;
                        screen.reasons = vec!["SCREENING_REPLACEMENT_UNAVAILABLE".into()];
                    }
                }
            }
        }
        if !self.is_terminal() {
            self.advance(decision_at);
        }
        self.revision += 1;
        Ok(())
    }

    fn resume_after_unaccepted_replacement(&mut self, now: DateTime<Utc>) -> Result<bool, String> {
        let screen = &self.checks[&CheckKind::Sanctions];
        let Some(current) = &screen.operation_id else {
            return Ok(false);
        };
        if self.screen_obligations.get(current).and_then(|h| h.outcome)
            != Some(ScreenOutcome::NotAccepted)
        {
            return Ok(false);
        }
        let Some(parent) = self
            .screen_obligations
            .values()
            .find(|h| h.replacement_operation.as_ref() == Some(current))
        else {
            return Ok(false);
        };
        if !screen.evidence.as_ref().is_some_and(|e| {
            e.evidence_ref == parent.original_evidence.evidence_ref
                && e.revision == parent.original_evidence.revision
        }) {
            return Ok(false);
        }
        let (Some(outcome), Some(observed), Some(received)) =
            (parent.outcome, parent.observed_at, parent.received_at)
        else {
            return Ok(false);
        };
        if outcome == ScreenOutcome::NotAccepted {
            // Both vendor IDs are now fenced. A new capture starts a distinct requirement.
            self.checks
                .get_mut(&CheckKind::Document)
                .expect("strict screening requires document")
                .status = CheckStatus::AwaitingEvidence;
            self.checks.get_mut(&CheckKind::Document).unwrap().reasons =
                vec!["FRESH_SCREENING_IDENTITY_REQUIRED".into()];
            self.advance(now);
            self.revision += 1;
            return Ok(true);
        }
        let operation = parent.original_operation.clone();
        let check = self.checks.get_mut(&CheckKind::Sanctions).unwrap();
        check.operation_id = Some(operation.clone());
        check.evidence = Some(parent.original_evidence.clone());
        check.fact = None;
        check.review = parent.review.clone();
        if let Some(review) = &check.review {
            check.status = if review.action == ReviewAction::Clear {
                CheckStatus::Passed
            } else {
                CheckStatus::Failed
            };
        } else if matches!(outcome, ScreenOutcome::Clear | ScreenOutcome::Hit) {
            check.status = CheckStatus::Pending;
            self.record_received(
                CheckKind::Sanctions,
                &operation,
                Observation::Sanctions {
                    hit: outcome == ScreenOutcome::Hit,
                },
                observed,
                Some(received),
                now,
            )?;
            return Ok(true);
        } else {
            check.status = CheckStatus::Review;
            check.reasons = vec!["ORIGINAL_SCREENING_REVIEW".into()];
        }
        self.advance(now);
        self.revision += 1;
        Ok(true)
    }

    pub fn review_original(
        &mut self,
        operation: &str,
        action: ReviewAction,
        actor: &str,
        reason: &str,
        review_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        if self.is_terminal()
            || ![actor, review_ref].into_iter().all(valid_reference)
            || !super::wire::valid_review_reason(reason)
        {
            return Err("invalid screening disposition".into());
        }
        let hold = self
            .screen_obligations
            .get_mut(operation)
            .filter(|h| h.awaiting_disposition())
            .ok_or("original screen is not awaiting review")?;
        let review = ReviewRecord {
            kind: CheckKind::Sanctions,
            operation_id: operation.into(),
            action,
            actor: actor.into(),
            reason: reason.into(),
            review_ref: review_ref.into(),
            recorded_at: now,
        };
        hold.review = Some(review.clone());
        self.reviews.push(review.clone());
        let screen = self.checks.get_mut(&CheckKind::Sanctions).unwrap();
        if screen.operation_id.as_deref() == Some(operation) {
            screen.status = if action == ReviewAction::Clear {
                CheckStatus::Passed
            } else {
                CheckStatus::Failed
            };
            screen.reasons = vec![if action == ReviewAction::Clear {
                "SANCTIONS_CLEARED_BY_REVIEW"
            } else {
                "SANCTIONS_CONFIRMED"
            }
            .into()];
            screen.review = Some(review);
        }
        self.advance(now);
        self.revision += 1;
        Ok(())
    }
}

use super::{
    BindingSnapshot, BoundEvidence, CaseState, CheckStatus, Fact, Origin, Policy, ProductCheck,
    ReviewRecord,
};
use crate::{valid_reference, CheckKind, Observation};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProductCase {
    pub case_id: String,
    pub tenant_id: String,
    pub subject_ref: String,
    pub policy: Policy,
    pub bindings: BindingSnapshot,
    pub checks: BTreeMap<CheckKind, ProductCheck>,
    pub state: CaseState,
    pub revision: i64,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub reviews: Vec<ReviewRecord>,
}

impl ProductCase {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        case_id: String,
        tenant_id: String,
        subject_ref: String,
        policy: Policy,
        evidence: BTreeMap<CheckKind, BoundEvidence>,
        candidates: &[Fact],
        bindings: BindingSnapshot,
        now: DateTime<Utc>,
    ) -> Result<Self, String> {
        policy.validate()?;
        if ![&case_id, &tenant_id, &subject_ref]
            .iter()
            .all(|s| valid_reference(s))
        {
            return Err("invalid case, tenant or subject reference".into());
        }
        Self::validate_bindings(&tenant_id, &subject_ref, &policy, &bindings)?;
        for (&kind, asset) in &evidence {
            let rule = policy
                .checks
                .iter()
                .find(|r| r.kind == kind)
                .ok_or("evidence outside product scope")?;
            if kind == CheckKind::Sanctions
                || !asset.matches(&tenant_id, &subject_ref, rule, &bindings)
            {
                return Err("evidence binding does not satisfy this requirement".into());
            }
        }
        let mut checks = BTreeMap::new();
        for rule in &policy.checks {
            let kind = rule.kind;
            let mut check = ProductCheck::empty(kind);
            if let Some(asset) = evidence.get(&kind) {
                // Explicitly submitted evidence takes precedence over a historical observation.
                check.start(&case_id, asset.clone());
            } else if let Some(fact) = rule.latest_candidate(
                candidates,
                &case_id,
                &tenant_id,
                &subject_ref,
                &bindings,
                now,
            ) {
                if rule.accepts_source(fact, now) {
                    check.origin = Some(Origin::Reused);
                    check.status = CheckStatus::Passed;
                    check.operation_id = Some(fact.operation_id.clone());
                    check.fact = Some(fact.clone());
                    check.evidence = Some(fact.evidence.clone());
                    check.reasons.clear();
                }
            }
            checks.insert(kind, check);
        }
        let mut case = Self {
            case_id,
            tenant_id,
            subject_ref,
            policy,
            bindings,
            checks,
            revision: 1,
            state: CaseState::Running,
            created_at: now,
            decided_at: None,
            reviews: Vec::new(),
        };
        case.advance(now);
        Ok(case)
    }

    fn validate_bindings(
        tenant: &str,
        subject: &str,
        policy: &Policy,
        bindings: &BindingSnapshot,
    ) -> Result<(), String> {
        if [
            &bindings.current_document_revision,
            &bindings.current_portrait_revision,
        ]
        .into_iter()
        .flatten()
        .any(|rev| !valid_reference(rev))
        {
            return Err("invalid current identity revision".into());
        }
        if let (Some(asset), Some(rule)) = (
            &bindings.screening,
            policy
                .checks
                .iter()
                .find(|r| r.kind == CheckKind::Sanctions),
        ) {
            if !asset.matches(tenant, subject, rule, bindings) {
                return Err("invalid screening identity binding".into());
            }
        }
        Ok(())
    }

    pub fn validate_current_bindings(&self, bindings: &BindingSnapshot) -> Result<(), String> {
        Self::validate_bindings(&self.tenant_id, &self.subject_ref, &self.policy, bindings)
    }

    /// Final check of a proposed, unpublished approval while the subject's fact
    /// generation is locked. A published decision is never passed to this method.
    pub fn verify_proposed_approval(&mut self, facts: &[Fact], at: DateTime<Utc>) {
        if self.state != CaseState::Approved || !self.has_reused_checks() {
            return;
        }
        let version = self.revision;
        self.state = CaseState::Running;
        self.decided_at = None;
        self.revalidate_reuse(facts, at);
        // These changes belong to the same unpublished revision.
        self.revision = version;
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.state, CaseState::Approved | CaseState::Rejected)
    }

    pub fn has_reused_checks(&self) -> bool {
        self.checks
            .values()
            .any(|check| check.origin == Some(Origin::Reused))
    }

    /// Revalidate pinned originals against the authoritative subject facts at a decision boundary.
    /// New positive observations do not reset the age of a case's original observation.
    pub fn revalidate_reuse(&mut self, candidates: &[Fact], now: DateTime<Utc>) {
        if self.is_terminal() {
            return;
        }
        let mut changed = false;
        for rule in &self.policy.checks {
            let check = self
                .checks
                .get_mut(&rule.kind)
                .expect("check from pinned plan");
            if check.origin != Some(Origin::Reused) {
                continue;
            }
            let Some(original) = &check.fact else {
                continue;
            };
            let still_published = candidates.iter().any(|candidate| candidate == original);
            let superseded = candidates.iter().any(|candidate| {
                candidate.observed_at >= original.observed_at
                    && rule.matches_candidate(
                        candidate,
                        &self.case_id,
                        &self.tenant_id,
                        &self.subject_ref,
                        &self.bindings,
                        now,
                    )
                    && !rule.accepts_source(candidate, now)
            });
            if original.received_at.is_none() || !still_published || superseded {
                check.clear("REUSE_SUPERSEDED");
                changed = true;
            }
        }
        let revision = self.revision;
        self.refresh(now);
        if changed && self.revision == revision {
            self.revision += 1;
        }
    }

    pub fn refresh_bindings(
        &mut self,
        bindings: BindingSnapshot,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        if self.is_terminal() {
            return Ok(());
        }
        Self::validate_bindings(&self.tenant_id, &self.subject_ref, &self.policy, &bindings)?;
        let changed = self.bindings != bindings;
        self.bindings = bindings;
        let revision = self.revision;
        self.refresh(now);
        if changed && revision == self.revision {
            self.revision += 1;
        }
        Ok(())
    }

    pub fn refresh(&mut self, now: DateTime<Utc>) {
        if self.is_terminal() {
            return;
        }
        let previous = self.checks.clone();
        self.advance(now);
        if previous != self.checks {
            self.revision += 1;
        }
    }

    pub(super) fn advance(&mut self, now: DateTime<Utc>) {
        for rule in &self.policy.checks {
            let check = self
                .checks
                .get_mut(&rule.kind)
                .expect("check from pinned plan");
            let binding_changed = check.evidence.as_ref().is_some_and(|asset| {
                !asset.matches(&self.tenant_id, &self.subject_ref, rule, &self.bindings)
                    || (rule.kind == CheckKind::Sanctions
                        && !self
                            .bindings
                            .screening
                            .as_ref()
                            .is_some_and(|current| asset.same_screening_identity(current)))
            });
            if binding_changed {
                check.clear("EVIDENCE_SUPERSEDED");
                continue;
            }
            if check.status != CheckStatus::Passed || check.review.is_some() {
                continue;
            }
            if let Some(fact) = &check.fact {
                if check.origin == Some(Origin::Reused) && !rule.reuse_allows(fact, now) {
                    check.clear("REUSE_EXPIRED");
                } else {
                    let assessment = rule.assess(&fact.observation, now);
                    if assessment.status != CheckStatus::Passed {
                        check.status = assessment.status;
                        check.reasons = assessment.reasons;
                    }
                }
            }
        }
        let doc_passed = self
            .checks
            .get(&CheckKind::Document)
            .is_some_and(|doc| doc.status == CheckStatus::Passed);
        if let Some(screen) = self.checks.get_mut(&CheckKind::Sanctions) {
            // A confirmed candidate remains actionable even if a different prerequisite expires.
            if !doc_passed && !matches!(screen.status, CheckStatus::Failed | CheckStatus::Review) {
                screen.clear("VERIFIED_IDENTITY_REQUIRED");
            } else if doc_passed
                && screen.operation_id.is_none()
                && screen.status != CheckStatus::Blocked
            {
                if let Some(identity) = self.bindings.screening.clone() {
                    screen.start(&self.case_id, identity);
                } else {
                    screen.status = CheckStatus::Blocked;
                    screen.reasons = vec!["SCREENING_IDENTITY_UNAVAILABLE".into()];
                }
            }
        }
        use CaseState::*;
        let has = |status| self.checks.values().any(|c| c.status == status);
        self.state =
            if has(CheckStatus::Failed) {
                Rejected
            } else if has(CheckStatus::Review) {
                AwaitingReview
            } else if has(CheckStatus::Blocked) {
                ProviderBlocked
            } else if self.checks.values().any(|c| {
                c.kind != CheckKind::Sanctions && c.status == CheckStatus::AwaitingEvidence
            }) {
                AwaitingEvidence
            } else if has(CheckStatus::Pending) {
                Running
            } else {
                Approved
            };
        if self.is_terminal() {
            self.decided_at = Some(now);
        }
    }

    pub fn fresh_passes(&self) -> Vec<Fact> {
        self.checks
            .values()
            .filter(|check| {
                check.status == CheckStatus::Passed
                    && check.origin == Some(Origin::Fresh)
                    && check.review.is_none()
            })
            .filter_map(|check| check.fact.clone())
            .collect()
    }

    /// Publish identity findings that actually contradict a pass. An outage or malformed observation
    /// says nothing about the subject and therefore cannot supersede a prior identity check.
    pub fn fresh_adverse(&self) -> Vec<Fact> {
        self.checks
            .values()
            .filter(|check| {
                check.origin == Some(Origin::Fresh) && check.status == CheckStatus::AwaitingEvidence
            })
            .filter_map(|check| check.fact.as_ref())
            .filter(|fact| {
                !fact.source_passed
                    && matches!(
                        fact.observation,
                        Observation::Document { .. } | Observation::Face { .. }
                    )
            })
            .cloned()
            .collect()
    }
}

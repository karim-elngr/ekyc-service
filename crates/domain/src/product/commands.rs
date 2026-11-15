use super::{BoundEvidence, CheckStatus, Fact, ProductCase, ReviewAction, ReviewRecord};
use crate::{valid_reference, CheckKind, Observation};
use chrono::{DateTime, Utc};

impl ProductCase {
    pub fn provide(&mut self, evidence: BoundEvidence, now: DateTime<Utc>) -> Result<(), String> {
        if self.is_terminal() {
            return Err("case is terminal".into());
        }
        let rule = self
            .policy
            .checks
            .iter()
            .find(|r| r.kind == evidence.kind)
            .ok_or("check outside product scope")?;
        if evidence.kind == CheckKind::Sanctions
            || !evidence.matches(&self.tenant_id, &self.subject_ref, rule, &self.bindings)
        {
            return Err("evidence binding does not satisfy this requirement".into());
        }
        let check = self
            .checks
            .get_mut(&evidence.kind)
            .expect("check from pinned plan");
        if check.status != CheckStatus::AwaitingEvidence {
            return Err("check is not awaiting evidence".into());
        }
        if check.evidence.as_ref().is_some_and(|old| {
            old.revision == evidence.revision && old.evidence_ref == evidence.evidence_ref
        }) {
            return Err("corrected evidence must have a new revision or reference".into());
        }
        check.start(&self.case_id, evidence);
        self.advance(now);
        self.revision += 1;
        Ok(())
    }

    pub fn retry(&mut self, kind: CheckKind, now: DateTime<Utc>) -> Result<(), String> {
        if self.is_terminal() {
            return Err("case is terminal".into());
        }
        let check = self
            .checks
            .get_mut(&kind)
            .ok_or("check outside product scope")?;
        if check.status != CheckStatus::Blocked {
            return Err("check is not blocked".into());
        }
        let evidence = if kind == CheckKind::Sanctions {
            self.bindings.screening.clone()
        } else {
            check.evidence.clone()
        }
        .ok_or("current evidence unavailable")?;
        check.start(&self.case_id, evidence);
        self.advance(now);
        self.revision += 1;
        Ok(())
    }

    pub fn record(
        &mut self,
        kind: CheckKind,
        operation_id: &str,
        observation: Observation,
        observed_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        self.record_received(kind, operation_id, observation, observed_at, Some(now), now)
    }

    pub fn record_received(
        &mut self,
        kind: CheckKind,
        operation_id: &str,
        observation: Observation,
        observed_at: DateTime<Utc>,
        received_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        self.record_result(
            kind,
            operation_id,
            observation,
            observed_at,
            received_at,
            now,
            None,
        )
    }

    /// The vendor answered, but the issuer could not validate its current evidence binding.
    pub fn binding_failure(
        &mut self,
        kind: CheckKind,
        operation_id: &str,
        observed_at: DateTime<Utc>,
        received_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
        code: &str,
    ) -> Result<(), String> {
        if !matches!(
            code,
            "EVIDENCE_REGISTRY_UNAVAILABLE" | "INVALID_EVIDENCE_BINDING"
        ) {
            return Err("invalid evidence registry failure".into());
        }
        self.record_result(
            kind,
            operation_id,
            Observation::Unavailable,
            observed_at,
            received_at,
            now,
            Some(code),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn record_result(
        &mut self,
        kind: CheckKind,
        operation_id: &str,
        observation: Observation,
        observed_at: DateTime<Utc>,
        received_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
        binding_error: Option<&str>,
    ) -> Result<(), String> {
        if self.is_terminal() {
            return Ok(());
        }
        let rule = self
            .policy
            .checks
            .iter()
            .find(|r| r.kind == kind)
            .ok_or("check outside product scope")?;
        let check = self.checks.get_mut(&kind).expect("check from pinned plan");
        if check.status != CheckStatus::Pending
            || check.operation_id.as_deref() != Some(operation_id)
        {
            return Ok(());
        }
        let evidence = check
            .evidence
            .clone()
            .ok_or("missing dispatched evidence binding")?;
        // Providers and the case worker may run on different clocks. Preserve the
        // result for investigation, but never base approval on a future observation.
        let assessment = if observed_at > received_at.unwrap_or(now) {
            super::Assessment {
                status: CheckStatus::Blocked,
                reasons: vec!["OBSERVATION_TIME_INVALID".into()],
            }
        } else {
            rule.assess(&observation, now)
        };
        check.fact = Some(Fact {
            source_passed: assessment.status == CheckStatus::Passed,
            source_product: self.policy.product.clone(),
            origin_case_id: self.case_id.clone(),
            operation_id: operation_id.into(),
            provider: evidence.provider.clone(),
            evidence,
            observed_at,
            received_at,
            observation,
        });
        check.status = assessment.status;
        check.reasons = binding_error
            .map(|code| vec![code.into()])
            .unwrap_or(assessment.reasons);
        self.advance(now);
        self.revision += 1;
        Ok(())
    }

    pub fn review(
        &mut self,
        kind: CheckKind,
        action: ReviewAction,
        actor: &str,
        reason: &str,
        review_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        if self.is_terminal() {
            return Err("case is terminal".into());
        }
        if ![actor, review_ref].into_iter().all(valid_reference)
            || !super::wire::valid_review_reason(reason)
        {
            return Err("review requires actor, reason and reference".into());
        }
        let check = self
            .checks
            .get_mut(&kind)
            .ok_or("check outside product scope")?;
        if kind != CheckKind::Sanctions
            || check.status != CheckStatus::Review
            || !check
                .fact
                .as_ref()
                .is_some_and(|f| matches!(f.observation, Observation::Sanctions { hit: true }))
        {
            return Err("no potential sanctions match to resolve".into());
        }
        let record = ReviewRecord {
            kind,
            operation_id: check
                .operation_id
                .clone()
                .ok_or("missing reviewed operation")?,
            action,
            actor: actor.into(),
            reason: reason.into(),
            review_ref: review_ref.into(),
            recorded_at: now,
        };
        check.review = Some(record.clone());
        self.reviews.push(record);
        match action {
            ReviewAction::Clear => {
                check.status = CheckStatus::Passed;
                check.reasons = vec!["SANCTIONS_CLEARED_BY_REVIEW".into()];
            }
            ReviewAction::Confirm => {
                check.status = CheckStatus::Failed;
                check.reasons = vec!["SANCTIONS_CONFIRMED".into()];
            }
        }
        self.advance(now);
        self.revision += 1;
        Ok(())
    }
}

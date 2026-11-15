use crate::checks::{CheckJob, ProviderChecksClient};
use crate::settings::RetrySettings;
use chrono::{DateTime, Utc};
use ekyc_domain::{
    evaluate, summarize_checks, CheckKind, Decision, Observation, OpenCase, POLICY_VERSION,
};
use ekyc_postgres::{CaseView, Store};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

const STATE: &str = "case_v1";

#[derive(Clone, Serialize, Deserialize)]
struct Case {
    request: OpenCase,
    deadline_ms: i64,
    revision: i64,
    policy_version: String,
    scope: Vec<CheckKind>,
    selected: BTreeMap<CheckKind, Observation>,
    conclusion: Option<(DateTime<Utc>, Decision)>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CompletedCheck {
    pub check: CheckKind,
    pub observation: Observation,
}

pub(crate) struct VerificationCase {
    pub store: Store,
    pub retry: RetrySettings,
}

#[restate_sdk::object(client_visibility = "pub(crate)")]
impl VerificationCase {
    #[handler]
    async fn open(&self, ctx: ObjectContext<'_>, input: Json<OpenCase>) -> HandlerResult<()> {
        let request = input.into_inner();
        if let Some(existing) = ctx.get::<Json<Case>>(STATE).await? {
            if existing.0.request == request {
                return Ok(());
            }
            return Err(
                TerminalError::new_with_code(409, "key already used for another command").into(),
            );
        }
        request
            .validate()
            .map_err(|message| TerminalError::new_with_code(422, message))?;
        // Capture the clock and current retry configuration once for this case.
        let retry = self.retry;
        let (now, retry) = ctx
            .run(move || async move { Ok(Json((Utc::now(), retry))) })
            .name("pin-case-settings")
            .await?
            .into_inner();
        let deadline_ms = now.timestamp_millis() + retry.case_deadline_ms as i64;
        let scope = request.purpose.checks().to_vec();
        let observations = scope
            .iter()
            .copied()
            .filter(|kind| !request.evidence.contains_key(kind))
            .map(|kind| (kind, Observation::Missing))
            .collect();
        let mut case = Case {
            revision: 1,
            policy_version: POLICY_VERSION.into(),
            scope,
            selected: observations,
            conclusion: None,
            request,
            deadline_ms,
        };
        case.evaluate(now)?;
        self.persist(&ctx, &case).await?;
        if case.conclusion.is_some() {
            return Ok(());
        }

        // Delayed expiry runs independently of every provider job and its retries.
        let remaining = remaining_ms(deadline_ms, clock(&ctx).await?);
        ctx.object_client::<VerificationCaseClient>(ctx.key())
            .expire()
            .send_after(Duration::from_millis(remaining));
        for (&kind, evidence_ref) in &case.request.evidence {
            let job = CheckJob {
                case_id: ctx.key().to_owned(),
                kind,
                evidence_ref: evidence_ref.clone(),
                deadline_ms,
                retry,
            };
            ctx.service_client::<ProviderChecksClient>()
                .run(Json(job))
                .send();
        }
        Ok(())
    }

    /// Repair the latest read model for an existing case through the operator ingress.
    /// The SQL version guard makes a shared repair safe alongside a newer exclusive write.
    #[handler]
    async fn reproject(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<()> {
        let case = ctx
            .get::<Json<Case>>(STATE)
            .await?
            .ok_or_else(|| TerminalError::new_with_code(404, "case does not exist"))?;
        let view = case.0.project(ctx.key());
        let store = self.store.clone();
        ctx.run(move || async move {
            store.publish(&view).await?;
            Ok(())
        })
        .name("repair-latest-projection")
        .await?;
        Ok(())
    }

    #[handler(ingress_private)]
    async fn active(
        &self,
        ctx: SharedObjectContext<'_>,
        kind: Json<CheckKind>,
    ) -> HandlerResult<bool> {
        let case = ctx.get::<Json<Case>>(STATE).await?;
        Ok(case.is_some_and(|case| {
            case.0.conclusion.is_none()
                && case.0.scope.contains(&kind.0)
                && !case.0.selected.contains_key(&kind.0)
        }))
    }

    #[handler(ingress_private)]
    async fn record(
        &self,
        ctx: ObjectContext<'_>,
        result: Json<CompletedCheck>,
    ) -> HandlerResult<()> {
        let mut case = ctx
            .get::<Json<Case>>(STATE)
            .await?
            .ok_or_else(|| TerminalError::new_with_code(404, "case does not exist"))?
            .into_inner();
        let result = result.into_inner();
        if !case.scope.contains(&result.check) {
            return Err(TerminalError::new_with_code(422, "check outside case scope").into());
        }
        if case.selected.contains_key(&result.check) || case.conclusion.is_some() {
            // Preserve a late result separately. It never becomes a selected decision fact.
            ctx.set(&format!("late_{}", result.check.as_str()), Json(result));
            return Ok(());
        }
        let now = clock(&ctx).await?;
        if now.timestamp_millis() >= case.deadline_ms {
            ctx.set(&format!("late_{}", result.check.as_str()), Json(result));
            case.close_unresolved();
        } else {
            case.selected.insert(result.check, result.observation);
        }
        case.revision += 1;
        case.evaluate(now)?;
        self.persist(&ctx, &case).await
    }

    #[handler(ingress_private)]
    async fn expire(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        let Some(case) = ctx.get::<Json<Case>>(STATE).await? else {
            return Ok(());
        };
        let mut case = case.into_inner();
        if case.conclusion.is_some() {
            return Ok(());
        }
        let now = clock(&ctx).await?;
        let remaining = remaining_ms(case.deadline_ms, now);
        if remaining > 0 {
            ctx.object_client::<VerificationCaseClient>(ctx.key())
                .expire()
                .send_after(Duration::from_millis(remaining));
            return Ok(());
        }
        case.close_unresolved();
        case.revision += 1;
        case.evaluate(now)?;
        self.persist(&ctx, &case).await
    }
}

impl VerificationCase {
    async fn persist(&self, ctx: &ObjectContext<'_>, case: &Case) -> HandlerResult<()> {
        ctx.set(STATE, Json(case.clone()));
        let view = case.project(ctx.key());
        let store = self.store.clone();
        ctx.run(move || async move {
            store.publish(&view).await?;
            Ok(())
        })
        .name("publish-versioned-case")
        .await?;
        Ok(())
    }
}

async fn clock(ctx: &ObjectContext<'_>) -> HandlerResult<DateTime<Utc>> {
    Ok(ctx
        .run(|| async { Ok(Json(Utc::now())) })
        .name("decision-clock")
        .await?
        .into_inner())
}

pub(crate) fn remaining_ms(deadline_ms: i64, now: DateTime<Utc>) -> u64 {
    deadline_ms.saturating_sub(now.timestamp_millis()).max(0) as u64
}

impl Case {
    fn close_unresolved(&mut self) {
        for &kind in &self.scope {
            self.selected
                .entry(kind)
                .or_insert(Observation::Unavailable);
        }
    }

    fn evaluate(&mut self, now: DateTime<Utc>) -> HandlerResult<()> {
        // Keep the dispatch explicit when introducing v2; existing cases stay on v1.
        if self.policy_version != "v1" {
            return Err(TerminalError::new("unsupported stored policy version").into());
        }
        if let Some(decision) = evaluate(&self.selected, &self.scope, now.date_naive()) {
            self.conclusion = Some((now, decision));
        }
        Ok(())
    }

    fn project(&self, case_id: &str) -> CaseView {
        let state = match (&self.conclusion, self.revision) {
            (Some(_), _) => "DECIDED",
            (None, 1) => "ACCEPTED",
            (None, _) => "RUNNING",
        };
        let decided_at = self.conclusion.as_ref().map(|(time, _)| *time);
        let evaluated_on = decided_at.map(|time| time.date_naive());
        CaseView {
            case_id: case_id.to_owned(),
            version: self.revision,
            state: state.into(),
            purpose: self.request.purpose,
            scope: self.scope.clone(),
            checks: summarize_checks(&self.selected, &self.scope, evaluated_on),
            observations: self.selected.clone(),
            decision: self
                .conclusion
                .as_ref()
                .map(|(_, decision)| decision.clone()),
            policy_version: self.policy_version.clone(),
            evaluated_on,
            decided_at,
        }
    }
}

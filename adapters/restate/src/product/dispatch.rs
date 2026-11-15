use super::{clock, Flight, Operation, ProductCase, ProductCaseClient, Stored, STATE};
use crate::checks::{CheckJob, ProviderChecksClient};
use chrono::{DateTime, Utc};
use ekyc_domain::{
    product::{CheckStatus, ProductCase as Case},
    CheckKind,
};
use restate_sdk::prelude::*;
use std::{collections::BTreeMap, time::Duration};

/// Preserve a pending investigation when revalidation replaces its underlying identity.
pub(super) fn superseded_screen<'a>(
    old: &'a Case,
    current: &Case,
) -> Option<&'a ekyc_domain::product::Fact> {
    let old_screen = old.checks.get(&CheckKind::Sanctions)?;
    if old_screen.status != CheckStatus::Review {
        return None;
    }
    let fact = old_screen.fact.as_ref()?;
    let current_op = current
        .checks
        .get(&CheckKind::Sanctions)
        .and_then(|screen| screen.operation_id.as_deref());
    (current_op != Some(&fact.operation_id)).then_some(fact)
}

impl Stored {
    pub(super) fn pending(&self, op: &Operation) -> bool {
        !self.case.is_terminal()
            && self.case.checks.get(&op.kind).is_some_and(|check| {
                check.status == CheckStatus::Pending
                    && check.operation_id.as_deref() == Some(op.id.as_str())
            })
    }

    pub(super) fn schedule(&mut self, now: DateTime<Utc>) -> Vec<CheckJob> {
        let mut flights = BTreeMap::new();
        let mut jobs = Vec::new();
        if !self.case.is_terminal() {
            for (&kind, check) in &self.case.checks {
                if check.status != CheckStatus::Pending {
                    continue;
                }
                let Some((op, evidence)) = check.operation_id.as_ref().zip(check.evidence.as_ref())
                else {
                    continue;
                };
                let flight = match self
                    .flights
                    .remove(&kind)
                    .filter(|flight| &flight.operation_id == op)
                {
                    Some(flight) => flight,
                    None => {
                        let budget = if check.is_interactive() {
                            crate::settings::INTERACTIVE_DEADLINE_MS
                        } else {
                            self.retry.case_deadline_ms as i64
                        };
                        let deadline_ms = now.timestamp_millis() + budget;
                        jobs.push(CheckJob {
                            operation_id: op.clone(),
                            callback: check.is_interactive(),
                            provider: evidence.provider.clone(),
                            case_id: self.case.case_id.clone(),
                            kind,
                            evidence_ref: evidence.provider_ref.clone(),
                            deadline_ms,
                            retry: self.retry,
                        });
                        Flight {
                            operation_id: op.clone(),
                            deadline_ms,
                            callback_invocation: None,
                            expiry_invocation: None,
                        }
                    }
                };
                flights.insert(kind, flight);
            }
        }
        self.flights = flights;
        jobs
    }
}

impl ProductCase {
    pub(super) async fn publish(
        &self,
        ctx: &ObjectContext<'_>,
        stored: &mut Stored,
        now: DateTime<Utc>,
    ) -> HandlerResult<()> {
        let store = self.store.clone();
        let proposal = stored.case.clone();
        stored.case = ctx
            .run(move || async move { Ok(Json(store.commit_assessment(&proposal).await?)) })
            .name("commit-product-assessment")
            .await?
            .into_inner();
        let previous = ctx.get::<Json<Stored>>(STATE).await?;
        let orphaned_match = previous
            .as_ref()
            .and_then(|old| superseded_screen(&old.0.case, &stored.case));
        let retired: Vec<_> = stored
            .flights
            .iter()
            .filter(|(kind, flight)| {
                !stored.pending(&Operation {
                    kind: **kind,
                    id: flight.operation_id.clone(),
                })
            })
            .flat_map(|(kind, flight)| {
                // Keep an in-flight sanctions response available for investigation, including after closure.
                let callback = (*kind != CheckKind::Sanctions)
                    .then_some(flight.callback_invocation.as_ref())
                    .flatten();
                callback.into_iter().chain(flight.expiry_invocation.iter())
            })
            .filter(|id| id.as_str() != ctx.invocation_id())
            .cloned()
            .collect();
        let jobs = stored.schedule(now);
        for id in retired {
            ctx.invocation_handle(id).cancel();
        }
        ctx.set(STATE, Json(stored.clone()));
        if let Some(fact) = orphaned_match {
            self.alert(
                ctx,
                &stored.case,
                &fact.operation_id,
                fact.observed_at,
                fact.received_at,
            )
            .await?;
        }
        if !jobs.is_empty() {
            let dispatch_at = clock(ctx).await?;
            for job in jobs {
                let deadline = crate::settings::remaining_ms(job.deadline_ms, dispatch_at);
                let op = Operation {
                    kind: job.kind,
                    id: job.operation_id.clone(),
                };
                let interactive = job.callback;
                let kind = job.kind;
                let provider = ctx
                    .service_client::<ProviderChecksClient>()
                    .product(Json(job))
                    .send();
                let expiry = ctx
                    .object_client::<ProductCaseClient>(ctx.key())
                    .expire(Json(op))
                    .send_after(Duration::from_millis(deadline));
                if interactive {
                    let flight = stored.flights.get_mut(&kind).expect("scheduled flight");
                    flight.callback_invocation = Some(provider.await?.invocation_id().to_owned());
                    flight.expiry_invocation = Some(expiry.await?.invocation_id().to_owned());
                    ctx.set(STATE, Json(stored.clone()));
                }
            }
        }
        Ok(())
    }
}

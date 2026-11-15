use crate::interactive::InteractiveMocks;
use crate::product::{Operation, ProductCaseClient, ProductResult};
use crate::settings::{remaining_ms, RetrySettings};
use chrono::{DateTime, Utc};
use ekyc_domain::{CheckKind, Observation};
use ekyc_ports::{Evaluated, Provider, ProviderError, ProviderErrorKind, ScreeningLookup};
use ekyc_postgres::{Gate, ProviderLimits, Store};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CheckJob {
    pub case_id: String,
    pub kind: CheckKind,
    pub evidence_ref: String,
    #[serde(default)]
    pub callback: bool,
    pub provider: String,
    pub operation_id: String,
    pub deadline_ms: i64,
    pub retry: RetrySettings,
}

#[derive(Serialize, Deserialize)]
enum Attempt {
    Deadline,
    GateWait(u64),
    Uncertain,
    Called(Result<Evaluated<Observation>, ProviderError>, DateTime<Utc>),
}

pub(crate) struct ProviderChecks {
    pub store: Store,
    pub providers: BTreeMap<(CheckKind, String), Arc<dyn Provider>>,
    pub interactive: InteractiveMocks,
    pub limits: BTreeMap<CheckKind, ProviderLimits>,
}

#[restate_sdk::service(ingress_private, client_visibility = "pub(crate)")]
impl ProviderChecks {
    #[handler]
    async fn product(&self, ctx: Context<'_>, input: Json<CheckJob>) -> HandlerResult<()> {
        let job = input.into_inner();
        let operation = Operation {
            kind: job.kind,
            id: job.operation_id.clone(),
        };
        let mut uncertain_screen = false;
        let key = (job.kind, job.provider.clone());
        let result = match self.providers.get(&key) {
            None => Some((None, None)),
            Some(provider) if job.callback => {
                if !ctx
                    .object_client::<ProductCaseClient>(&job.case_id)
                    .active(Json(operation.clone()))
                    .call()
                    .await?
                {
                    return Ok(());
                }
                let evaluation = self
                    .interactive
                    .check(&ctx, &job, provider.as_ref())
                    .await?;
                Some((evaluation, None))
            }
            Some(provider) => {
                self.perform(&ctx, &job, provider, &mut uncertain_screen)
                    .await?
            }
        };
        if let Some((evaluation, received_at)) = result {
            let received_at = match received_at {
                Some(time) => time,
                None => clock(&ctx).await?,
            };
            let (observation, observed_at) = evaluation
                .map(|e| (e.result, e.evaluated_at))
                .unwrap_or((Observation::Unavailable, received_at));
            ctx.object_client::<ProductCaseClient>(&job.case_id)
                .record(Json(ProductResult {
                    operation,
                    observation,
                    observed_at,
                    received_at: Some(received_at),
                }))
                .send();
        }
        if uncertain_screen {
            ctx.service_client::<ProviderChecksClient>()
                .reconcile(Json(OutstandingScreen {
                    case_id: job.case_id,
                    provider: job.provider,
                    operation_id: job.operation_id,
                    poll: 0,
                }))
                .send();
        }
        Ok(())
    }

    /// Each pending lookup schedules its successor durably, outside the case's lifetime.
    #[handler]
    async fn reconcile(
        &self,
        ctx: Context<'_>,
        screen: Json<OutstandingScreen>,
    ) -> HandlerResult<()> {
        let mut screen = screen.into_inner();
        let provider = self
            .providers
            .get(&(CheckKind::Sanctions, screen.provider.clone()))
            .cloned();
        let id = screen.operation_id.clone();
        let lookup = ctx
            .run(move || async move {
                let result = match provider {
                    Some(p) => p.screening_result(&id).await.ok(),
                    None => None,
                };
                Ok(Json((result, Utc::now())))
            })
            .name("lookup-original-screening-operation")
            .await?
            .into_inner();
        match lookup {
            (Some(ScreeningLookup::Complete(result)), received_at) => {
                ctx.object_client::<ProductCaseClient>(&screen.case_id)
                    .record(Json(ProductResult {
                        operation: Operation {
                            kind: CheckKind::Sanctions,
                            id: screen.operation_id,
                        },
                        observation: result.result,
                        observed_at: result.evaluated_at,
                        received_at: Some(received_at),
                    }))
                    .send();
            }
            (Some(ScreeningLookup::NotAccepted | ScreeningLookup::Inconclusive), _) => {}
            _ => {
                let delay = Duration::from_secs((1u64 << screen.poll.min(9)).min(300));
                screen.poll = screen.poll.saturating_add(1);
                ctx.service_client::<ProviderChecksClient>()
                    .reconcile(Json(screen))
                    .send_after(delay);
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct OutstandingScreen {
    case_id: String,
    provider: String,
    operation_id: String,
    poll: u32,
}

async fn clock(ctx: &Context<'_>) -> HandlerResult<DateTime<Utc>> {
    Ok(ctx
        .run(|| async { Ok(Json(Utc::now())) })
        .name("product-receipt-clock")
        .await?
        .into_inner())
}

impl ProviderChecks {
    async fn perform(
        &self,
        ctx: &Context<'_>,
        job: &CheckJob,
        provider: &Arc<dyn Provider>,
        uncertain_screen: &mut bool,
    ) -> HandlerResult<Option<(Option<Evaluated<Observation>>, Option<DateTime<Utc>>)>> {
        let limits = self.limits[&job.kind];
        let mut observed_at = None;
        let mut attempt_number = 1;
        let observation = loop {
            let active = ctx
                .object_client::<ProductCaseClient>(&job.case_id)
                .active(Json(Operation {
                    kind: job.kind,
                    id: job.operation_id.clone(),
                }))
                .call()
                .await?;
            if !active {
                return Ok(None);
            }
            // An operation is one verification at the vendor. A lease is one HTTP attempt.
            let token = format!("{}/{attempt_number}", job.operation_id);
            let result = self
                .attempt(ctx, job, provider.clone(), token.clone())
                .await?;
            let delay = match result {
                Attempt::Deadline => {
                    // Replay may reach this deadline after the vendor accepted a request
                    // but before its result was journaled. Lookup may safely fence an
                    // operation that never reached the vendor.
                    *uncertain_screen = job.kind == CheckKind::Sanctions;
                    break None;
                }
                Attempt::GateWait(wait_ms) => wait_ms.max(25),
                Attempt::Uncertain => {
                    *uncertain_screen = job.kind == CheckKind::Sanctions;
                    // The prior permit expired after an ambiguous I/O boundary. Retry with
                    // a new lease only because all four configured vendors deduplicate operation.
                    if attempt_number >= job.retry.max_attempts {
                        break None;
                    }
                    attempt_number += 1;
                    job.retry.retry_base_ms
                }
                Attempt::Called(outcome, time) => {
                    observed_at = Some(time);
                    // The vendor result is already in the journal before releasing the lease.
                    let error = outcome.as_ref().err().copied();
                    if error.is_some_and(|e| e.kind != ProviderErrorKind::Throttled)
                        && job.kind == CheckKind::Sanctions
                    {
                        *uncertain_screen = true;
                    }
                    let failed = error.is_some_and(|e| e.kind == ProviderErrorKind::Transient);
                    let hint = error.and_then(|e| {
                        e.retry_after_ms.or_else(|| {
                            (e.kind == ProviderErrorKind::Throttled)
                                .then_some(job.retry.retry_base_ms)
                        })
                    });
                    let store = self.store.clone();
                    let kind = job.kind;
                    ctx.run(move || async move {
                        store
                            .finish_provider_with_backoff(kind, &token, failed, limits, hint)
                            .await?;
                        Ok(())
                    })
                    .name("release-provider-lease")
                    .await?;
                    match outcome {
                        Ok(facts) => {
                            *uncertain_screen = false;
                            break Some(facts);
                        }
                        Err(error)
                            if error.kind == ProviderErrorKind::Permanent
                                || attempt_number >= job.retry.max_attempts =>
                        {
                            break None
                        }
                        Err(error) => {
                            let delay = retry_delay(
                                job.retry.retry_base_ms,
                                attempt_number,
                                ctx.random_seed(),
                                error.retry_after_ms,
                            );
                            attempt_number += 1;
                            delay
                        }
                    }
                }
            };
            let now = ctx
                .run(|| async { Ok(Json(Utc::now())) })
                .name("retry-clock")
                .await?
                .into_inner();
            let remaining = remaining_ms(job.deadline_ms, now);
            if remaining == 0 {
                break None;
            }
            // At the deadline the independent case timer settles any outstanding checks.
            ctx.sleep(Duration::from_millis(delay.min(remaining)))
                .await?;
        };

        Ok(Some((observation, observed_at)))
    }

    async fn attempt(
        &self,
        ctx: &Context<'_>,
        job: &CheckJob,
        provider: Arc<dyn Provider>,
        token: String,
    ) -> HandlerResult<Attempt> {
        let store = self.store.clone();
        let limits = self.limits[&job.kind];
        let job = job.clone();
        Ok(ctx
            .run(move || async move {
                if remaining_ms(job.deadline_ms, Utc::now()) == 0 {
                    return Ok(Json(Attempt::Deadline));
                }
                let acquiring = std::time::Instant::now();
                let result = match store.acquire_provider(job.kind, &token, limits).await? {
                    Gate::Wait { retry_after_ms } => Attempt::GateWait(retry_after_ms),
                    Gate::Closed => Attempt::Uncertain,
                    Gate::Granted => {
                        let remaining = remaining_ms(job.deadline_ms, Utc::now());
                        // SQL may have granted a lease long before its response reached us.
                        let lease_remaining = Duration::from_millis(limits.lease_ms)
                            .saturating_sub(acquiring.elapsed());
                        let io_budget = Duration::from_millis(remaining).min(lease_remaining);
                        let outcome = if io_budget.is_zero() {
                            Err(ProviderError {
                                kind: ProviderErrorKind::Transient,
                                retry_after_ms: None,
                            })
                        } else {
                            tokio::time::timeout(
                                io_budget,
                                provider.check(&job.evidence_ref, &job.operation_id),
                            )
                            .await
                            .unwrap_or(Err(ProviderError {
                                kind: ProviderErrorKind::Transient,
                                retry_after_ms: None,
                            }))
                        };
                        Attempt::Called(outcome, Utc::now())
                    }
                };
                // A provider business/transport error is a successful journaled outcome.
                // Only a DB infrastructure error above requests a retry from Restate itself.
                Ok(Json(result))
            })
            .name("acquire-and-call-provider")
            .await?
            .into_inner())
    }
}

fn retry_delay(base: u64, attempt: u32, seed: u64, retry_after: Option<u64>) -> u64 {
    let exponential = base.saturating_mul(1_u64 << attempt.saturating_sub(1).min(16));
    // Equal jitter, deterministic for a replayed invocation.
    let half = exponential / 2;
    let mixed = seed.rotate_left(attempt % 64);
    let jittered = exponential.saturating_sub(half) + mixed % (half + 1);
    jittered.max(retry_after.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::retry_delay;

    #[test]
    fn provider_hint_and_deadline_do_not_overflow_backoff() {
        assert_eq!(retry_delay(250, 3, 99, Some(u64::MAX)), u64::MAX);
        assert!((500..=1000).contains(&retry_delay(250, 3, 99, None)));
    }
}

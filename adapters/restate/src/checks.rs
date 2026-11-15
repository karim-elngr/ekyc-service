use crate::case::{remaining_ms, CompletedCheck, VerificationCaseClient};
use crate::settings::RetrySettings;
use chrono::Utc;
use ekyc_domain::{CheckKind, Observation};
use ekyc_ports::{Provider, ProviderError, ProviderErrorKind};
use ekyc_postgres::{Gate, ProviderLimits, Store};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CheckJob {
    pub case_id: String,
    pub kind: CheckKind,
    pub evidence_ref: String,
    pub deadline_ms: i64,
    pub retry: RetrySettings,
}

#[derive(Serialize, Deserialize)]
enum Attempt {
    Deadline,
    GateWait(u64),
    Uncertain,
    Called(Result<Observation, ProviderError>),
}

pub(crate) struct ProviderChecks {
    pub store: Store,
    pub providers: BTreeMap<CheckKind, Arc<dyn Provider>>,
    pub limits: BTreeMap<CheckKind, ProviderLimits>,
}

#[restate_sdk::service(ingress_private, client_visibility = "pub(crate)")]
impl ProviderChecks {
    #[handler]
    async fn run(&self, ctx: Context<'_>, input: Json<CheckJob>) -> HandlerResult<()> {
        let job = input.into_inner();
        let case = ctx.object_client::<VerificationCaseClient>(&job.case_id);
        let operation = format!("{}/{}", job.case_id, job.kind.as_str());
        let provider = self
            .providers
            .get(&job.kind)
            .ok_or_else(|| TerminalError::new("provider not configured"))?;
        let limits = self.limits[&job.kind];
        let mut attempt_number = 1;
        let observation = loop {
            if !case.active(Json(job.kind)).call().await? {
                return Ok(());
            }
            // An operation is one verification at the vendor. A lease is one HTTP attempt.
            let token = format!("{operation}/{attempt_number}");
            let result = self
                .attempt(
                    &ctx,
                    &job,
                    provider.clone(),
                    limits,
                    operation.clone(),
                    token.clone(),
                )
                .await?;
            let delay = match result {
                Attempt::Deadline => break Observation::Unavailable,
                Attempt::GateWait(wait_ms) => wait_ms.max(25),
                Attempt::Uncertain => {
                    // The prior permit expired after an ambiguous I/O boundary. Retry with
                    // a new lease only because all four configured vendors deduplicate operation.
                    if attempt_number >= job.retry.max_attempts {
                        break Observation::Unavailable;
                    }
                    attempt_number += 1;
                    job.retry.retry_base_ms
                }
                Attempt::Called(outcome) => {
                    // The vendor result is already in the journal before releasing the lease.
                    let error = outcome.as_ref().err().copied();
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
                        Ok(facts) => break facts,
                        Err(error)
                            if error.kind == ProviderErrorKind::Permanent
                                || attempt_number >= job.retry.max_attempts =>
                        {
                            break Observation::Unavailable
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
                break Observation::Unavailable;
            }
            // At the deadline the independent case timer settles any outstanding checks.
            ctx.sleep(Duration::from_millis(delay.min(remaining)))
                .await?;
        };

        case.record(Json(CompletedCheck {
            check: job.kind,
            observation,
        }))
        .call()
        .await?;
        Ok(())
    }
}

impl ProviderChecks {
    async fn attempt(
        &self,
        ctx: &Context<'_>,
        job: &CheckJob,
        provider: Arc<dyn Provider>,
        limits: ProviderLimits,
        operation: String,
        token: String,
    ) -> HandlerResult<Attempt> {
        let store = self.store.clone();
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
                                provider.check(&job.evidence_ref, &operation),
                            )
                            .await
                            .unwrap_or(Err(ProviderError {
                                kind: ProviderErrorKind::Transient,
                                retry_after_ms: None,
                            }))
                        };
                        Attempt::Called(outcome)
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

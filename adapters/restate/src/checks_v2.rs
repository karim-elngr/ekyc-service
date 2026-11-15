use crate::attempts::{dispatch, Action, Answer, Reply};
use crate::checks::{clock, retry_delay, CheckJob, ProviderChecks, ProviderChecksClient};
use crate::interactive::{Callback, Registration};
use crate::product::{
    Operation, ProductCaseClient, ProductResult, RouteResult, ScreenLookupResult,
};
use crate::settings::remaining_ms;
use chrono::{DateTime, Utc};
use ekyc_domain::{product::ScreenOutcome, CheckKind, Observation};
use ekyc_ports::{Evaluated, ProviderErrorKind, ScreeningLookup};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Recovery {
    job: CheckJob,
    sequence: u64,
    poll: u32,
}

fn routed_address(job: &CheckJob) -> bool {
    job.kind == CheckKind::Address && matches!(job.provider.as_str(), "address-a" | "address-b")
}

fn schedule_recovery(ctx: &Context<'_>, job: &CheckJob) {
    ctx.service_client::<ProviderChecksClient>()
        .reconcile_v2(Json(Recovery {
            job: job.clone(),
            sequence: 1,
            poll: 0,
        }))
        .idempotency_key(format!("{}/recover", job.operation_id))
        .send();
}

fn op(job: &CheckJob) -> Operation {
    Operation {
        kind: job.kind,
        id: job.operation_id.clone(),
    }
}

async fn record(
    ctx: &Context<'_>,
    job: &CheckJob,
    result: Option<Evaluated<Observation>>,
    received_at: DateTime<Utc>,
) -> HandlerResult<()> {
    if let (true, Some(found)) = (routed_address(job), result.as_ref()) {
        ctx.object_client::<ProductCaseClient>(&job.case_id)
            .route_result(Json(RouteResult {
                operation: op(job),
                observation: Some(found.result.clone()),
                status: "complete".into(),
                observed_at: found.evaluated_at,
                received_at,
            }))
            .call()
            .await?;
    } else if let (
        true,
        Some(Evaluated {
            result: Observation::Sanctions { hit },
            evaluated_at,
        }),
    ) = (
        matches!(job.provider.as_str(), "sanctions-a" | "sanctions-b")
            && job.kind == CheckKind::Sanctions,
        result.as_ref(),
    ) {
        ctx.object_client::<ProductCaseClient>(&job.case_id)
            .screen_lookup(Json(ScreenLookupResult {
                operation_id: job.operation_id.clone(),
                outcome: if *hit {
                    ScreenOutcome::Hit
                } else {
                    ScreenOutcome::Clear
                },
                observed_at: *evaluated_at,
                received_at,
            }))
            .call()
            .await?;
    } else {
        let (observation, observed_at) = result
            .map(|f| (f.result, f.evaluated_at))
            .unwrap_or((Observation::Unavailable, received_at));
        ctx.object_client::<ProductCaseClient>(&job.case_id)
            .record(Json(ProductResult {
                operation: op(job),
                observation,
                observed_at,
                received_at: Some(received_at),
                callback_invocation: job.callback.then(|| ctx.invocation_id().to_owned()),
            }))
            .call()
            .await?;
    }
    Ok(())
}

impl ProviderChecks {
    pub(crate) async fn run_v2(&self, ctx: &Context<'_>, job: CheckJob) -> HandlerResult<()> {
        let provider = self.providers.get(&(job.kind, job.provider.clone()));
        let mut callback = None;
        let action = if job.callback {
            let (awakeable_id, future) = ctx.awakeable::<Json<Callback>>();
            callback = Some(future);
            Action::Register(Registration {
                case_id: job.case_id.clone(),
                operation_id: job.operation_id.clone(),
                awakeable_id,
                invocation_id: ctx.invocation_id().to_owned(),
            })
        } else {
            Action::Submit
        };
        let mut sequence = 0;
        let mut sends = 0;
        let retrieval_supported =
            !job.callback && (routed_address(&job) || job.kind == CheckKind::Sanctions);
        let mut recovering = false;
        let mut refusal = String::new();
        loop {
            sequence += 1;
            let reply = dispatch(ctx, &self.rules, &job, action.clone(), sequence).await?;
            let delay = match reply {
                Reply::Called(Ok(Answer::Facts(facts)), at) => {
                    return record(ctx, &job, Some(facts), at).await
                }
                Reply::Called(Ok(Answer::Registered), _) => {
                    let callback = callback.take().expect("callback registration");
                    let now = clock(ctx).await?;
                    let result = restate_sdk::select! {
                        reply=callback => reply?.into_inner().observation(provider.expect("registered provider").as_ref()),
                        elapsed=ctx.sleep(Duration::from_millis(remaining_ms(job.deadline_ms,now))) => {elapsed?;None}
                    };
                    return record(ctx, &job, result, clock(ctx).await?).await;
                }
                Reply::Called(Ok(Answer::Lookup(_)), _) => {
                    return Err(TerminalError::new("unexpected lookup response").into())
                }
                Reply::Wait(reason, delay) => {
                    if routed_address(&job)
                        && reason == "breaker_open"
                        && ctx
                            .object_client::<ProductCaseClient>(&job.case_id)
                            .reroute(Json(op(&job)))
                            .call()
                            .await?
                    {
                        return Ok(());
                    }
                    refusal = reason;
                    delay
                }
                Reply::Blocked(reason) => {
                    if routed_address(&job)
                        && !recovering
                        && matches!(reason.as_str(), "paused" | "draining" | "share_cap")
                    {
                        if ctx
                            .object_client::<ProductCaseClient>(&job.case_id)
                            .reroute(Json(op(&job)))
                            .call()
                            .await?
                        {
                            return Ok(());
                        }
                        refusal = reason;
                        250
                    } else {
                        if refusal.is_empty() || reason != "deadline" {
                            refusal = reason;
                        }
                        break;
                    }
                }
                Reply::Unknown | Reply::Called(Err(_), _) => {
                    let error = match reply {
                        Reply::Called(Err(error), _) => Some(error),
                        _ => None,
                    };
                    refusal.clear();
                    sends += 1;
                    let uncertain = error.is_none_or(|e| e.kind != ProviderErrorKind::Throttled);
                    if uncertain && retrieval_supported && !recovering {
                        recovering = true;
                        schedule_recovery(ctx, &job);
                        if routed_address(&job) {
                            // Pilot policies either dispatch a linked replacement or await this lookup.
                            ctx.object_client::<ProductCaseClient>(&job.case_id)
                                .reroute(Json(op(&job)))
                                .call()
                                .await?;
                            return Ok(());
                        }
                        if matches!(job.provider.as_str(), "sanctions-a" | "sanctions-b") {
                            break;
                        }
                    }
                    if sends >= job.retry.max_attempts
                        || error.is_some_and(|e| e.kind == ProviderErrorKind::Permanent)
                    {
                        break;
                    }
                    retry_delay(
                        job.retry.retry_base_ms,
                        sends,
                        ctx.random_seed(),
                        error.and_then(|e| e.retry_after_ms),
                    )
                }
            };
            let now = clock(ctx).await?;
            let remaining = remaining_ms(job.deadline_ms, now);
            if remaining == 0 || (!refusal.is_empty() && delay >= remaining) {
                break;
            }
            ctx.sleep(Duration::from_millis(delay.max(1).min(remaining)))
                .await?;
        }
        // A throttled operation can exhaust its deadline or final attempt. Retrieve
        // its authoritative disposition before treating any possible submission as closed.
        if sends > 0 && retrieval_supported && !recovering {
            schedule_recovery(ctx, &job);
        }
        if routed_address(&job) && !refusal.is_empty() {
            let store = self.store.clone();
            let case = job.case_id.clone();
            let op = job.operation_id.clone();
            ctx.run(move || async move {
                store.block_unsent_route(&case, &op, &refusal).await?;
                Ok(())
            })
            .name("record-blocked-address-assignment")
            .await?;
        }
        record(ctx, &job, None, clock(ctx).await?).await
    }

    pub(crate) async fn recover_v2(
        &self,
        ctx: &Context<'_>,
        mut input: Recovery,
    ) -> HandlerResult<()> {
        let job = &input.job;
        let reply = dispatch(ctx, &self.rules, job, Action::Lookup, input.sequence).await?;
        let delay = match reply {
            Reply::Called(Ok(Answer::Lookup(lookup)), received_at)
                if !matches!(lookup, ScreeningLookup::Pending) =>
            {
                let (status, facts) = match lookup {
                    ScreeningLookup::NotAccepted => ("not_accepted", None),
                    ScreeningLookup::Inconclusive => ("inconclusive", None),
                    ScreeningLookup::Complete(f) => ("complete", Some(f)),
                    ScreeningLookup::Pending => unreachable!(),
                };
                if routed_address(job) {
                    let observed_at = facts.as_ref().map_or(received_at, |f| f.evaluated_at);
                    ctx.object_client::<ProductCaseClient>(&job.case_id)
                        .route_result(Json(RouteResult {
                            operation: op(job),
                            observation: facts.map(|f| f.result),
                            status: status.into(),
                            observed_at,
                            received_at,
                        }))
                        .call()
                        .await?;
                } else if job.kind == CheckKind::Sanctions
                    && matches!(job.provider.as_str(), "sanctions-a" | "sanctions-b")
                {
                    let observed_at = facts.as_ref().map_or(received_at, |f| f.evaluated_at);
                    let outcome = match (status, facts.map(|f| f.result)) {
                        ("not_accepted", _) => ScreenOutcome::NotAccepted,
                        ("inconclusive", _) => ScreenOutcome::Inconclusive,
                        (_, Some(Observation::Sanctions { hit: true })) => ScreenOutcome::Hit,
                        (_, Some(Observation::Sanctions { hit: false })) => ScreenOutcome::Clear,
                        _ => {
                            return Err(
                                TerminalError::new("invalid original screening result").into()
                            )
                        }
                    };
                    ctx.object_client::<ProductCaseClient>(&job.case_id)
                        .screen_lookup(Json(ScreenLookupResult {
                            operation_id: job.operation_id.clone(),
                            outcome,
                            observed_at,
                            received_at,
                        }))
                        .call()
                        .await?;
                } else if facts.is_some() {
                    record(ctx, job, facts, received_at).await?;
                }
                return Ok(());
            }
            Reply::Wait(_, delay) => delay,
            Reply::Blocked(_) => return Ok(()),
            _ => {
                let delay = (1000_u64 << input.poll.min(9)).min(300_000);
                input.poll = input.poll.saturating_add(1);
                delay
            }
        };
        input.sequence = input.sequence.saturating_add(1);
        ctx.service_client::<ProviderChecksClient>()
            .reconcile_v2(Json(input))
            .send_after(Duration::from_millis(delay.max(25)));
        Ok(())
    }
}

//! One scoped invocation performs at most one outbound HTTP request.
use crate::checks::CheckJob;
use crate::interactive::{InteractiveMocks, Registration};
use crate::rules::Rules;
use crate::settings::remaining_ms;
use chrono::{DateTime, Utc};
use ekyc_domain::{CheckKind, Observation};
use ekyc_ports::{Evaluated, Provider, ProviderError, ProviderErrorKind, ScreeningLookup};
use ekyc_postgres::{AttemptAdmission, AttemptIdentity, Store};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Action {
    Submit,
    Register(Registration),
    Lookup,
}

impl Action {
    pub fn lane(&self) -> &'static str {
        match self {
            Self::Submit | Self::Register(_) => "submit",
            Self::Lookup => "lookup",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Request {
    job: CheckJob,
    action: Action,
    token: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Answer {
    Facts(Evaluated<Observation>),
    Registered,
    Lookup(ScreeningLookup),
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Reply {
    Called(Result<Answer, ProviderError>, DateTime<Utc>),
    Blocked(String),
    Wait(String, u64),
    Unknown,
}

pub(crate) struct ProviderAttemptV2 {
    pub before_send_barrier: Option<std::path::PathBuf>,
    pub store: Store,
    pub rules: Rules,
    pub providers: BTreeMap<(CheckKind, String), Arc<dyn Provider>>,
    pub interactive: InteractiveMocks,
}

#[restate_sdk::service(ingress_private, client_visibility = "pub(crate)")]
impl ProviderAttemptV2 {
    #[handler]
    async fn execute(&self, ctx: Context<'_>, input: Json<Request>) -> HandlerResult<Json<Reply>> {
        let request = input.into_inner();
        let Some(pool) = self
            .rules
            .pool(request.job.kind, &request.job.provider)
            .map(str::to_owned)
        else {
            return Ok(Json(Reply::Blocked("missing_contract".into())));
        };
        if ctx.scope() != Some(&pool) || ctx.limit_key() != Some(request.action.lane()) {
            return Ok(Json(Reply::Blocked("missing_concurrency_scope".into())));
        }
        let Some(provider) = self
            .providers
            .get(&(request.job.kind, request.job.provider.clone()))
            .cloned()
        else {
            return Ok(Json(Reply::Blocked("missing_provider".into())));
        };
        let interactive = self.interactive.clone();
        let store = self.store.clone();
        let rules = self.rules.clone();
        let barrier = self.before_send_barrier.clone();
        Ok(ctx
            .run(move || async move {
                if !rules.effective(Some(&pool)).await {
                    return Ok(Json(Reply::Wait(
                        "concurrency_rules_unavailable".into(),
                        1000,
                    )));
                }
                let job = &request.job;
                let lane = request.action.lane();
                let claim = store
                    .authorize_attempt(AttemptIdentity {
                        token: &request.token,
                        case_id: &job.case_id,
                        operation: &job.operation_id,
                        kind: job.kind,
                        provider: &job.provider,
                        pool: &pool,
                        lane,
                        deadline_ms: job.deadline_ms,
                        max_attempts: job.retry.max_attempts,
                    })
                    .await?;
                let deadline = match claim {
                    AttemptAdmission::Wait { reason, retry_ms } => {
                        return Ok(Json(Reply::Wait(reason, retry_ms)))
                    }
                    AttemptAdmission::Blocked { reason } => {
                        return Ok(Json(Reply::Blocked(reason)))
                    }
                    AttemptAdmission::Unknown => return Ok(Json(Reply::Unknown)),
                    AttemptAdmission::Saved { result } => {
                        return Ok(Json(serde_json::from_value(result)?))
                    }
                    AttemptAdmission::Send { deadline_ms } => deadline_ms,
                };
                // Enabled only by the isolated crash drill. Its second connection observes the
                // committed permit while the vendor still has zero requests for this operation.
                if lane == "submit" {
                    if let Some(marker) = barrier.as_ref() {
                        while marker.is_file() && remaining_ms(deadline, Utc::now()) > 0 {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    }
                }
                let start = Instant::now();
                let physical_call =
                    async {
                        match &request.action {
                            Action::Submit => provider
                                .check(&job.evidence_ref, &job.operation_id)
                                .await
                                .map(Answer::Facts),
                            Action::Lookup => provider
                                .screening_result(&job.operation_id)
                                .await
                                .map(Answer::Lookup),
                            Action::Register(registration) => {
                                let endpoint = interactive.registrations.get(&job.kind).ok_or(
                                    ProviderError {
                                        kind: ProviderErrorKind::Permanent,
                                        retry_after_ms: None,
                                    },
                                )?;
                                interactive
                                    .transport
                                    .post_accepted(endpoint, &job.operation_id, registration)
                                    .await
                                    .map(|_| Answer::Registered)
                            }
                        }
                    };
                let remaining = remaining_ms(deadline, Utc::now());
                let (class, reply, hint) = if lane == "submit" && remaining == 0 {
                    ("expired", Reply::Blocked("deadline".into()), 0)
                } else {
                    let result = if lane == "lookup" {
                        physical_call.await
                    } else {
                        tokio::time::timeout(Duration::from_millis(remaining), physical_call)
                            .await
                            .unwrap_or(Err(ProviderError {
                                kind: ProviderErrorKind::Transient,
                                retry_after_ms: None,
                            }))
                    };
                    let class = match result.as_ref().err().map(|e| e.kind) {
                        None => "ok",
                        Some(ProviderErrorKind::Permanent) => "permanent",
                        Some(ProviderErrorKind::Transient) => "transient",
                        Some(ProviderErrorKind::Throttled) => "throttled",
                    };
                    let hint = result
                        .as_ref()
                        .err()
                        .and_then(|e| e.retry_after_ms)
                        .unwrap_or(job.retry.retry_base_ms);
                    (class, Reply::Called(result, Utc::now()), hint)
                };
                store
                    .finish_attempt(
                        &request.token,
                        class,
                        &reply,
                        start.elapsed().as_millis() as i64,
                        hint,
                    )
                    .await?;
                Ok(Json(reply))
            })
            .name("authorize-one-provider-attempt-and-send")
            .await?)
    }
}

pub(crate) async fn dispatch(
    ctx: &Context<'_>,
    rules: &Rules,
    job: &CheckJob,
    action: Action,
    sequence: u64,
) -> HandlerResult<Reply> {
    let Some(pool) = rules.pool(job.kind, &job.provider) else {
        return Ok(Reply::Blocked("missing_contract".into()));
    };
    let lane = action.lane();
    let request_id = format!("{}/{lane}/{sequence}", job.operation_id);
    let request = Request {
        job: job.clone(),
        action,
        token: request_id.clone(),
    };
    Ok(ctx
        .service_client::<ProviderAttemptV2Client>()
        .execute(Json(request))
        .scope(pool)
        .limit_key(lane)
        .idempotency_key(request_id)
        .call()
        .await?
        .into_inner())
}

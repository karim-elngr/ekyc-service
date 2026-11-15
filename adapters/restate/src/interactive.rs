use crate::checks::CheckJob;
use ekyc_domain::{CheckKind, Observation};
use ekyc_http_transport::Transport;
use ekyc_ports::{Evaluated, Provider, ProviderErrorKind};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::BTreeMap;

#[derive(Clone)]
pub(crate) struct InteractiveMocks {
    pub transport: Transport,
    pub registrations: BTreeMap<CheckKind, String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Registration {
    pub case_id: String,
    pub operation_id: String,
    pub awakeable_id: String,
    pub invocation_id: String,
}

#[derive(Deserialize)]
pub(crate) struct Callback {
    status: u16,
    body: Box<RawValue>,
}

impl Callback {
    pub fn observation(&self, provider: &dyn Provider) -> Option<Evaluated<Observation>> {
        if (200..300).contains(&self.status) {
            return provider.decode(self.body.get().as_bytes()).ok();
        }
        None
    }
}

impl InteractiveMocks {
    pub async fn check(
        &self,
        ctx: &Context<'_>,
        job: &CheckJob,
        provider: &dyn Provider,
    ) -> HandlerResult<Option<Evaluated<Observation>>> {
        let (awakeable_id, callback) = ctx.awakeable::<Json<Callback>>();
        let registration = Registration {
            case_id: job.case_id.clone(),
            operation_id: job.operation_id.clone(),
            awakeable_id,
            invocation_id: ctx.invocation_id().to_owned(),
        };
        let transport = self.transport.clone();
        let Some(endpoint) = self.registrations.get(&job.kind).cloned() else {
            return Ok(None);
        };
        let operation = job.operation_id.clone();
        // Restate replays the same callback id if the mock accepted a registration
        // before the HTTP acknowledgement was lost. The mock deduplicates it.
        let accepted = ctx
            .run(move || async move {
                match transport
                    .post_accepted(&endpoint, &operation, &registration)
                    .await
                {
                    Ok(()) => Ok(true),
                    Err(err) if err.kind == ProviderErrorKind::Permanent => Ok(false),
                    Err(err) => Err(err.into()),
                }
            })
            .name("register-provider-callback")
            .retry_policy(RunRetryPolicy::default().max_attempts(job.retry.max_attempts))
            .await;
        if !matches!(accepted, Ok(true)) {
            return Ok(None);
        }
        // Only a durable Restate wait remains; the HTTP call and its connection are finished.
        let now = ctx
            .run(|| async { Ok(Json(chrono::Utc::now())) })
            .name("callback-wait-clock")
            .await?
            .into_inner();
        let budget =
            std::time::Duration::from_millis(crate::settings::remaining_ms(job.deadline_ms, now));
        restate_sdk::select! {
            result = callback => Ok(result?.into_inner().observation(provider)),
            result = ctx.sleep(budget) => { result?; Ok(None) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ekyc_provider_sanctions::SanctionsProvider;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn callback_uses_the_vendor_schema_and_status() {
        let provider = SanctionsProvider::new(
            Transport::new(Duration::from_secs(2)).unwrap(),
            "http://unused".into(),
        );
        for (status, count, expected) in [
            (200, 1, Observation::Sanctions { hit: true }),
            (200, 0, Observation::Unavailable),
            (422, 1, Observation::Unavailable),
        ] {
            let wire = json!({"status": status, "body": {"evaluated_at":"2026-01-01T00:00:00Z","screening_status": "HIT", "match_count": count}});
            let callback: Callback = serde_json::from_str(&wire.to_string()).unwrap();
            assert_eq!(
                callback
                    .observation(&provider)
                    .map(|r| r.result)
                    .unwrap_or(Observation::Unavailable),
                expected
            );
        }
    }
}

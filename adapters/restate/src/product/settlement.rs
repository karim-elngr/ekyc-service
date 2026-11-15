use super::{clock, invalid, ProductCase, ProductResult, Selection, Stored, STATE};
use crate::evidence::EvidenceError;
use chrono::{DateTime, Utc};
use ekyc_domain::{product::ProductCase as Case, Observation};
use restate_sdk::prelude::*;
use std::collections::BTreeMap;

pub(super) fn settlement_bindings(
    case: &Case,
    bindings: ekyc_domain::product::BindingSnapshot,
) -> Result<ekyc_domain::product::BindingSnapshot, EvidenceError> {
    case.validate_current_bindings(&bindings)
        .map_err(|_| EvidenceError::Invalid)?;
    Ok(bindings)
}

pub(super) fn record_binding_failure(
    case: &mut Case,
    result: &ProductResult,
    now: DateTime<Utc>,
    error: EvidenceError,
) -> Result<(), String> {
    let code = match error {
        EvidenceError::Invalid => "INVALID_EVIDENCE_BINDING",
        EvidenceError::Unavailable => "EVIDENCE_REGISTRY_UNAVAILABLE",
    };
    case.binding_failure(
        result.operation.kind,
        &result.operation.id,
        result.observed_at,
        result.received_at,
        now,
        code,
    )
}

impl ProductCase {
    pub(super) async fn settle(
        &self,
        ctx: &ObjectContext<'_>,
        result: ProductResult,
    ) -> HandlerResult<()> {
        let Some(Json(mut stored)) = ctx.get::<Json<Stored>>(STATE).await? else {
            return Ok(());
        };
        let op = &result.operation;
        let before = stored.case.revision;
        let mut selection = Selection::Superseded;
        let positive = result.is_positive_screen();
        let mut invalid_time = false;
        if stored.pending(op) {
            let resolution = self
                .resolve(
                    ctx,
                    &stored.case.tenant_id,
                    &stored.case.subject_ref,
                    &BTreeMap::new(),
                )
                .await?;
            let bindings = match resolution.and_then(|(_, b)| settlement_bindings(&stored.case, b))
            {
                Ok(bindings) => bindings,
                Err(error) => {
                    let now = clock(ctx).await?;
                    record_binding_failure(&mut stored.case, &result, now, error)
                        .map_err(invalid)?;
                    self.publish(ctx, &mut stored, now).await?;
                    if positive {
                        self.alert(
                            ctx,
                            &stored.case,
                            &op.id,
                            result.observed_at,
                            result.received_at,
                        )
                        .await?;
                    }
                    return Ok(());
                }
            };
            let now = self.revalidate(ctx, &mut stored.case, bindings).await?;
            let deadline = stored
                .pending(op)
                .then(|| stored.flights[&op.kind].deadline_ms);
            selection = result.select(deadline);
            invalid_time = result.observed_at > result.received_at.unwrap_or(now);
            let observation = match selection {
                Selection::Current => Some(result.observation),
                Selection::TimedOut => Some(Observation::Unavailable),
                Selection::Superseded => None,
            };
            if let Some(observation) = observation {
                stored
                    .case
                    .record_received(
                        op.kind,
                        &op.id,
                        observation,
                        result.observed_at,
                        result.received_at,
                        now,
                    )
                    .map_err(invalid)?;
            }
            if stored.case.revision != before {
                self.publish(ctx, &mut stored, now).await?;
            }
        }
        if positive && (selection != Selection::Current || invalid_time) {
            // The alert references the published revision, including any identity
            // refresh that made this supplier operation obsolete.
            self.alert(
                ctx,
                &stored.case,
                &op.id,
                result.observed_at,
                result.received_at,
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn alert(
        &self,
        ctx: &ObjectContext<'_>,
        case: &Case,
        operation_id: &str,
        observed: DateTime<Utc>,
        received: Option<DateTime<Utc>>,
    ) -> HandlerResult<()> {
        let store = self.store.clone();
        let case = case.clone();
        let op = operation_id.to_owned();
        ctx.run(move || async move {
            store
                .late_product_screen(&case, &op, observed, received)
                .await?;
            Ok(())
        })
        .name("record-late-screening-alert")
        .await?;
        Ok(())
    }
}

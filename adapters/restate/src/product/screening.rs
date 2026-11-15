use super::{clock, invalid, ProductCase, ProductCaseClient, Stored};
use chrono::{DateTime, Utc};
use ekyc_domain::product::ScreenOutcome;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ScreenLookupResult {
    pub operation_id: String,
    pub outcome: ScreenOutcome,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
}

impl ProductCase {
    pub(super) async fn settle_original(
        &self,
        ctx: &ObjectContext<'_>,
        stored: &mut Stored,
        result: ScreenLookupResult,
    ) -> HandlerResult<()> {
        let Some(hold) = stored.case.screen_obligations.get(&result.operation_id) else {
            return Ok(());
        };
        if hold.outcome.is_some() {
            return Ok(());
        }
        if stored.case.is_terminal() {
            let store = self.store.clone();
            let case_id = stored.case.case_id.clone();
            ctx.run(move || async move {
                store
                    .closed_screen_lookup(
                        &case_id,
                        &result.operation_id,
                        result.outcome,
                        result.observed_at,
                        result.received_at,
                    )
                    .await?;
                Ok(())
            })
            .name("retain-closed-case-original-screen")
            .await?;
            return Ok(());
        }
        let before = stored.case.revision;
        let now = if result.outcome == ScreenOutcome::Hit {
            clock(ctx).await?
        } else {
            // A clear could release the final hold. Revalidate identity and reused facts
            // first, including when the result arrives after a deadline or replacement.
            let resolution = self
                .resolve(
                    ctx,
                    &stored.case.tenant_id,
                    &stored.case.subject_ref,
                    &BTreeMap::new(),
                )
                .await?;
            let Ok(bindings) = resolution
                .and_then(|(_, b)| super::settlement::settlement_bindings(&stored.case, b))
            else {
                ctx.object_client::<ProductCaseClient>(ctx.key())
                    .screen_lookup(Json(result))
                    .send_after(Duration::from_secs(30));
                return Ok(());
            };
            self.revalidate(ctx, &mut stored.case, bindings).await?
        };
        stored
            .case
            .record_screen_lookup(
                &result.operation_id,
                result.outcome,
                result.observed_at,
                result.received_at,
                now,
            )
            .map_err(invalid)?;
        if stored.case.revision != before {
            self.publish(ctx, stored, now).await?;
        }
        Ok(())
    }
}

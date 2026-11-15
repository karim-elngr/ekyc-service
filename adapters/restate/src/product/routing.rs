use super::{clock, invalid, Operation, ProductCase, RouteResult, Stored, STATE};
use ekyc_domain::{CheckKind, Observation};
use restate_sdk::prelude::*;

impl ProductCase {
    pub(super) async fn move_unsent_or_ambiguous(
        &self,
        ctx: &ObjectContext<'_>,
        op: Operation,
    ) -> HandlerResult<bool> {
        let Some(Json(mut stored)) = ctx.get::<Json<Stored>>(STATE).await? else {
            return Ok(false);
        };
        if op.kind != CheckKind::Address || !stored.case.routed_address() || !stored.pending(&op) {
            return Ok(false);
        }
        let now = clock(ctx).await?;
        let deadline = stored.flights[&op.kind].deadline_ms;
        if now.timestamp_millis() >= deadline || !stored.case.replace_address(&op.id, now) {
            return Ok(false);
        }
        let store = self.store.clone();
        let case = stored.case.clone();
        let choice = ctx
            .run(move || async move {
                Ok(Json(
                    store
                        .assign_route(&case, CheckKind::Address, deadline, true)
                        .await?,
                ))
            })
            .name("replace-address-assignment")
            .await?
            .into_inner();
        let Some((binding, _)) = choice else {
            return Ok(false);
        };
        stored
            .case
            .checks
            .get_mut(&op.kind)
            .expect("address check")
            .evidence = Some(binding);
        self.publish(ctx, &mut stored, now).await?;
        Ok(true)
    }

    pub(super) async fn apply_address_result(
        &self,
        ctx: &ObjectContext<'_>,
        result: RouteResult,
    ) -> HandlerResult<()> {
        let Some(Json(mut stored)) = ctx.get::<Json<Stored>>(STATE).await? else {
            return Ok(());
        };
        if !stored.case.routed_address()
            || result.operation.kind != CheckKind::Address
            || !matches!(
                (result.status.as_str(), &result.observation),
                ("complete", Some(Observation::Address { .. }))
                    | ("inconclusive" | "not_accepted", None)
            )
        {
            return Err(invalid("invalid routed address result"));
        }
        let store = self.store.clone();
        let case_id = stored.case.case_id.clone();
        let answers = ctx
            .run(move || async move {
                store
                    .finish_route(
                        &case_id,
                        &result.operation.id,
                        &result.status,
                        result.observation.as_ref(),
                        result.observed_at,
                        result.received_at,
                    )
                    .await?;
                Ok(Json(
                    store.route_answers(&case_id, &result.operation.id).await?,
                ))
            })
            .name("record-authoritative-address-result")
            .await?
            .into_inner();
        if stored.case.is_terminal() {
            return Ok(());
        }
        let before = stored.case.revision;
        let now = clock(ctx).await?;
        stored.case.settle_address(&answers, now).map_err(invalid)?;
        if before != stored.case.revision {
            self.publish(ctx, &mut stored, now).await?;
        }
        Ok(())
    }
}

//! Each product case owns its commands, provider attempts and pinned decision policy.
mod commands;
mod dispatch;
mod routing;
use commands::Command;
mod policies;
mod screening;
mod settlement;
pub(crate) use screening::ScreenLookupResult;
#[cfg(test)]
mod tests;
use crate::evidence::{EvidenceError, EvidenceResolver, EvidenceSnapshot};
use crate::settings::RetrySettings;
use chrono::{DateTime, Utc};
use ekyc_domain::product::{BindingSnapshot, BoundEvidence, Fact, ProductCase as Case};
use ekyc_domain::{CheckKind, Observation};
use ekyc_postgres::{ReuseQuery, Store};
pub(crate) use policies::Policies;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};

const STATE: &str = "product_case";
type EvidenceMap = BTreeMap<CheckKind, BoundEvidence>;
type Evidence = BTreeMap<CheckKind, String>;

#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    case: Case,
    opener: String,
    retry: RetrySettings,
    flights: BTreeMap<CheckKind, Flight>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Flight {
    operation_id: String,
    deadline_ms: i64,
    #[serde(default)]
    callback_invocation: Option<String>,
    #[serde(default)]
    expiry_invocation: Option<String>,
}

impl Flight {
    fn delivered_by(&mut self, operation: &str, caller: &str) {
        if self.operation_id == operation && self.callback_invocation.as_deref() == Some(caller) {
            self.callback_invocation = None;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Operation {
    pub kind: CheckKind,
    pub id: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RouteResult {
    pub operation: Operation,
    pub observation: Option<Observation>,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub status: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ProductResult {
    pub operation: Operation,
    pub observation: Observation,
    pub observed_at: DateTime<Utc>,
    #[serde(default)]
    pub received_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_invocation: Option<String>,
}

impl ProductResult {
    fn is_positive_screen(&self) -> bool {
        self.operation.kind == CheckKind::Sanctions
            && matches!(self.observation, Observation::Sanctions { hit: true })
    }

    fn select(&self, deadline: Option<i64>) -> Selection {
        match deadline {
            None => Selection::Superseded,
            Some(limit)
                if self
                    .received_at
                    .unwrap_or(self.observed_at)
                    .timestamp_millis()
                    >= limit =>
            {
                Selection::TimedOut
            }
            Some(_) => Selection::Current,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Selection {
    Current,
    TimedOut,
    Superseded,
}

fn already_received(seen_positive: Option<bool>, positive: bool) -> bool {
    match seen_positive {
        Some(true) => true,
        Some(false) => !positive,
        None => false,
    }
}

pub(crate) struct ProductCase {
    pub rules: crate::rules::Rules,
    pub store: Store,
    pub resolver: EvidenceResolver,
    pub retry: RetrySettings,
    pub policies: Policies,
    pub catalog: crate::providers::Catalog,
}

#[restate_sdk::object(client_visibility = "pub(crate)")]
impl ProductCase {
    #[handler]
    async fn command(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<Command>,
    ) -> HandlerResult<Json<Value>> {
        self.execute_command(ctx, input.into_inner()).await
    }

    /// Probe command registration, worker execution and authenticated issuer resolution.
    #[handler]
    async fn readiness_v2(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<Json<String>> {
        let issuer = self.resolver.clone();
        let rules = self.rules.clone();
        let ready = ctx
            .run(move || async move { Ok(issuer.ready().await && rules.effective(None).await) })
            .await?;
        if !ready {
            return Err(TerminalError::new_with_code(503, "evidence_registry_unavailable").into());
        }
        Ok(Json("ekyc.commands.v2".to_owned()))
    }

    /// Rebuild the current projection from the authoritative case after a read-model loss.
    /// Historical revisions and independent late alerts are restored from the database backup.
    #[handler]
    async fn reproject(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<()> {
        let stored = ctx
            .get::<Json<Stored>>(STATE)
            .await?
            .ok_or_else(|| TerminalError::new_with_code(404, "case_not_found"))?
            .into_inner();
        let store = self.store.clone();
        ctx.run(move || async move {
            store.restore_product_projection(&stored.case).await?;
            Ok(())
        })
        .name("repair-product-projection")
        .await?;
        Ok(())
    }

    #[handler(ingress_private)]
    async fn reroute(&self, ctx: ObjectContext<'_>, op: Json<Operation>) -> HandlerResult<bool> {
        self.move_unsent_or_ambiguous(&ctx, op.into_inner()).await
    }

    #[handler(ingress_private)]
    async fn route_result(
        &self,
        ctx: ObjectContext<'_>,
        result: Json<RouteResult>,
    ) -> HandlerResult<()> {
        self.apply_address_result(&ctx, result.into_inner()).await
    }

    #[handler(ingress_private)]
    async fn active(
        &self,
        ctx: SharedObjectContext<'_>,
        op: Json<Operation>,
    ) -> HandlerResult<bool> {
        Ok(ctx
            .get::<Json<Stored>>(STATE)
            .await?
            .is_some_and(|state| state.0.pending(&op.0)))
    }

    #[handler(ingress_private)]
    async fn record(
        &self,
        ctx: ObjectContext<'_>,
        result: Json<ProductResult>,
    ) -> HandlerResult<()> {
        let result = result.into_inner();
        let key = format!("result/{}", result.operation.id);
        let seen_positive = ctx.get::<bool>(&key).await?;
        let positive = result.is_positive_screen();
        // A delivery duplicate may follow a deadline or an earlier unavailable
        // result. The first actual positive must still be visible to operations.
        if already_received(seen_positive, positive) {
            return Ok(());
        }
        self.settle(&ctx, result).await?;
        ctx.set(&key, positive);
        Ok(())
    }

    #[handler(ingress_private)]
    async fn screen_lookup(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<ScreenLookupResult>,
    ) -> HandlerResult<()> {
        if let Some(Json(mut stored)) = ctx.get::<Json<Stored>>(STATE).await? {
            self.settle_original(&ctx, &mut stored, input.into_inner())
                .await?;
        }
        Ok(())
    }

    #[handler(ingress_private)]
    async fn assign_screen(
        &self,
        ctx: ObjectContext<'_>,
        operation: Json<String>,
    ) -> HandlerResult<()> {
        if let Some(Json(mut stored)) = ctx.get::<Json<Stored>>(STATE).await? {
            let now = clock(&ctx).await?;
            let operation = operation.into_inner();
            if let Some(hold) = stored.case.screen_obligations.get(&operation) {
                let delay = hold.assign_at.signed_duration_since(now);
                if hold.assign_at > now {
                    ctx.object_client::<ProductCaseClient>(ctx.key())
                        .assign_screen(Json(operation))
                        .send_after(Duration::from_millis(delay.num_milliseconds() as u64 + 1));
                } else if stored.case.assign_original(&operation, now) {
                    self.publish(&ctx, &mut stored, now).await?;
                }
            }
        }
        Ok(())
    }

    #[handler(ingress_private)]
    async fn expire(&self, ctx: ObjectContext<'_>, op: Json<Operation>) -> HandlerResult<()> {
        let op = op.into_inner();
        let Some(Json(stored)) = ctx.get::<Json<Stored>>(STATE).await? else {
            return Ok(());
        };
        if !stored.pending(&op) {
            return Ok(());
        }
        let now = clock(&ctx).await?;
        let delay = crate::settings::remaining_ms(stored.flights[&op.kind].deadline_ms, now);
        if delay > 0 {
            ctx.object_client::<ProductCaseClient>(ctx.key())
                .expire(Json(op))
                .send_after(Duration::from_millis(delay));
            return Ok(());
        }
        self.settle(
            &ctx,
            ProductResult {
                operation: op,
                observation: Observation::Unavailable,
                observed_at: now,
                received_at: None,
                callback_invocation: None,
            },
        )
        .await
    }
}

impl ProductCase {
    async fn candidates(
        &self,
        ctx: &ObjectContext<'_>,
        query: ReuseQuery,
    ) -> HandlerResult<Vec<Fact>> {
        let store = self.store.clone();
        Ok(ctx
            .run(move || async move { Ok(Json(store.reuse_candidates(&query).await?)) })
            .name("query-subject-facts")
            .await?
            .into_inner())
    }

    pub(super) async fn revalidate(
        &self,
        ctx: &ObjectContext<'_>,
        case: &mut Case,
        mut bindings: BindingSnapshot,
    ) -> HandlerResult<DateTime<Utc>> {
        bindings.for_policy(&case.policy);
        // Empty issuer reads revalidate identity; address proof alternatives remain pinned to their revision.
        if let Some(pinned) = case.bindings.provider_candidates.get(&CheckKind::Address) {
            bindings
                .provider_candidates
                .entry(CheckKind::Address)
                .or_insert_with(|| pinned.clone());
        }
        let now = clock(ctx).await?;
        case.refresh_bindings(bindings, now).map_err(invalid)?;
        if case.has_reused_checks() {
            let facts = self.candidates(ctx, ReuseQuery::pinned(case, now)).await?;
            case.revalidate_reuse(&facts, now);
        }
        Ok(now)
    }

    pub(super) async fn resolve(
        &self,
        ctx: &ObjectContext<'_>,
        tenant: &str,
        subject: &str,
        refs: &Evidence,
    ) -> HandlerResult<Result<(BTreeMap<CheckKind, BoundEvidence>, BindingSnapshot), EvidenceError>>
    {
        let (tenant, subject, refs) = (tenant.to_owned(), subject.to_owned(), refs.clone());
        let resolver = self.resolver.clone();
        let catalog = self.catalog.clone();
        Ok(ctx
            .run(move || async move {
                let resolved = resolver
                    .resolve(&tenant, &subject, &refs)
                    .await
                    .and_then(|snap| validated(snap, &catalog));
                Ok(Json(resolved))
            })
            .name("resolve-subject-evidence")
            .await?
            .into_inner())
    }
}

fn validated(
    mut snap: EvidenceSnapshot,
    catalog: &crate::providers::Catalog,
) -> Result<(BTreeMap<CheckKind, BoundEvidence>, BindingSnapshot), EvidenceError> {
    for items in snap.provider_candidates.values_mut() {
        items.retain_mut(|asset| catalog.bind(asset).is_ok());
    }
    for asset in snap.evidence.values_mut().chain(snap.screening.iter_mut()) {
        if catalog.bind(asset).is_err() {
            let replacement = snap
                .provider_candidates
                .get(&asset.kind)
                .and_then(|items| items.first());
            *asset = replacement.ok_or(EvidenceError::Invalid)?.clone();
        }
    }
    Ok((
        snap.evidence,
        BindingSnapshot {
            provider_candidates: snap.provider_candidates,
            current_document_revision: snap.current_document_revision,
            current_portrait_revision: snap.current_portrait_revision,
            screening: snap.screening,
        },
    ))
}

fn evidence_error(error: EvidenceError) -> HandlerError {
    match error {
        EvidenceError::Invalid => invalid("invalid evidence binding"),
        EvidenceError::Unavailable => {
            TerminalError::new_with_code(503, "evidence_registry_unavailable").into()
        }
    }
}

fn review_operation(expected: Option<&str>, submitted: &str) -> Result<(), String> {
    if expected == Some(submitted) {
        Ok(())
    } else {
        Err("review operation has changed".into())
    }
}

fn invalid(message: impl Into<String>) -> HandlerError {
    TerminalError::new_with_code(422, message).into()
}
fn conflict(message: impl Into<String>) -> HandlerError {
    TerminalError::new_with_code(409, message).into()
}
async fn clock(ctx: &ObjectContext<'_>) -> HandlerResult<DateTime<Utc>> {
    Ok(ctx
        .run(|| async { Ok(Json(Utc::now())) })
        .name("product-clock")
        .await?
        .into_inner())
}

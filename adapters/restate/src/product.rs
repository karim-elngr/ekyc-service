//! Each product case owns its commands, provider attempts and pinned decision policy.
mod commands;
mod dispatch;
use commands::Command;
mod policies;
mod settlement;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Operation {
    pub kind: CheckKind,
    pub id: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ProductResult {
    pub operation: Operation,
    pub observation: Observation,
    pub observed_at: DateTime<Utc>,
    #[serde(default)]
    pub received_at: Option<DateTime<Utc>>,
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
        let ready = ctx
            .run(move || async move { Ok(issuer.ready().await) })
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
        bindings: BindingSnapshot,
    ) -> HandlerResult<DateTime<Utc>> {
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
    for asset in snap.evidence.values_mut().chain(snap.screening.iter_mut()) {
        catalog.bind(asset)?;
    }
    Ok((
        snap.evidence,
        BindingSnapshot {
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

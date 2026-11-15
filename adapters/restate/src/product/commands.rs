use super::{
    clock, conflict, evidence_error, invalid, review_operation, Case, ProductCase, Stored, STATE,
};
pub(crate) use ekyc_domain::product::wire::Command;
use ekyc_domain::{
    product::wire::{Action, Evidence, Open, Retry, Review},
    valid_reference,
};
use restate_sdk::prelude::*;
use serde_json::Value;
use std::collections::BTreeMap;

impl ProductCase {
    pub(super) async fn execute_command(
        &self,
        ctx: ObjectContext<'_>,
        command: Command,
    ) -> HandlerResult<Json<Value>> {
        let existing = ctx.get::<Json<Stored>>(STATE).await?.map(Json::into_inner);
        if let Some(stored) = &existing {
            let allowed = stored.case.tenant_id == command.institution
                && stored.case.policy.product == command.product
                && (stored.opener == command.caller
                    || matches!(&command.action, Action::Review(_)));
            if !allowed {
                return Err(TerminalError::new_with_code(404, "case_not_found").into());
            }
            if let Some(receipt) = ctx.get::<Json<Command>>(&command.receipt_key()).await? {
                if receipt.0 == command {
                    return Ok(Json(stored.case.view()));
                }
                return Err(conflict("idempotency_key_reused"));
            }
        }
        let (mut stored, now) = match (existing, &command.action) {
            (Some(_), Action::Open(_)) => return Err(conflict("case_already_open")),
            (
                None,
                Action::Open(Open {
                    subject_ref,
                    use_case,
                    evidence,
                }),
            ) => {
                if !valid_reference(subject_ref) {
                    return Err(invalid("invalid subject reference"));
                }
                let selected = self.policies.select(&command.product, use_case);
                let retry = self.retry;
                let (policy, retry) = ctx
                    .run(move || async move { Ok(Json((selected.map_err(invalid)?, retry))) })
                    .name("pin-product-policy")
                    .await?
                    .into_inner();
                let (resolved, bindings) = self
                    .resolve(&ctx, &command.institution, subject_ref, evidence)
                    .await?
                    .map_err(evidence_error)?;
                let needs_candidates = policy
                    .checks
                    .iter()
                    .any(|rule| rule.reuse.is_some() && !resolved.contains_key(&rule.kind));
                let now = clock(&ctx).await?;
                let facts = if needs_candidates {
                    self.candidates(
                        &ctx,
                        ekyc_postgres::ReuseQuery {
                            case_id: ctx.key().into(),
                            institution: command.institution.clone(),
                            subject: subject_ref.clone(),
                            policy: policy.clone(),
                            bindings: bindings.clone(),
                            now,
                            pinned: None,
                        },
                    )
                    .await?
                } else {
                    Vec::new()
                };
                let case = Case::open(
                    ctx.key().into(),
                    command.institution.clone(),
                    subject_ref.clone(),
                    policy,
                    resolved,
                    &facts,
                    bindings,
                    now,
                )
                .map_err(invalid)?;
                (
                    Stored {
                        case,
                        opener: command.caller.clone(),
                        retry,
                        flights: BTreeMap::new(),
                    },
                    now,
                )
            }
            (None, _) => return Err(TerminalError::new_with_code(404, "case_not_found").into()),
            (Some(mut stored), action) => {
                if stored.case.is_terminal() {
                    return Err(conflict("case_already_decided"));
                }
                let empty = BTreeMap::new();
                let evidence = match action {
                    Action::Evidence(Evidence { evidence }) if evidence.is_empty() => {
                        return Err(invalid("empty evidence"))
                    }
                    Action::Evidence(Evidence { evidence }) => evidence,
                    _ => &empty,
                };
                let (resolved, bindings) = self
                    .resolve(
                        &ctx,
                        &stored.case.tenant_id,
                        &stored.case.subject_ref,
                        evidence,
                    )
                    .await?
                    .map_err(evidence_error)?;
                let revision = stored.case.revision;
                let now = self.revalidate(&ctx, &mut stored.case, bindings).await?;
                // Resolving the current identity may invalidate prior findings even when
                // the requested retry or review no longer applies. Do not lose that change,
                // or publish a partially accepted batch of replacement evidence.
                let refreshed = (revision != stored.case.revision).then(|| stored.case.clone());
                let applied = match action {
                    Action::Evidence(_) => resolved
                        .into_values()
                        .try_for_each(|item| stored.case.provide(item, now)),
                    Action::Retry(Retry { check }) => stored.case.retry(*check, now),
                    Action::Review(Review {
                        check,
                        disposition,
                        operation_id,
                        reason,
                        review_ref,
                    }) => {
                        let selected = stored
                            .case
                            .checks
                            .get(check)
                            .and_then(|step| step.operation_id.as_deref());
                        review_operation(selected, operation_id).and_then(|()| {
                            stored.case.review(
                                *check,
                                *disposition,
                                &command.caller,
                                reason,
                                review_ref,
                                now,
                            )
                        })
                    }
                    Action::Open(_) => unreachable!(),
                };
                if let Err(reason) = applied {
                    if let Some(case) = refreshed {
                        stored.case = case;
                        self.publish(&ctx, &mut stored, now).await?;
                    }
                    return Err(conflict(if reason == "review operation has changed" {
                        "stale_operation"
                    } else {
                        "invalid_case_state"
                    }));
                }
                (stored, now)
            }
        };
        self.publish(&ctx, &mut stored, now).await?;
        ctx.set(&command.receipt_key(), Json(command));
        Ok(Json(stored.case.view()))
    }
}

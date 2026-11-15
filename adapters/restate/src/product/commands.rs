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
                let selected =
                    self.policies
                        .select(&command.product, use_case)
                        .and_then(|policy| {
                            if (policy.strict_screen.is_some()
                                || policy.checks.iter().any(|r| r.routing.is_some()))
                                && std::env::var("EKYC_ROUTING_INSTITUTION").ok().as_deref()
                                    != Some(&command.institution)
                            {
                                return Err(
                                    "provider pilot is disabled for this institution".into()
                                );
                            }
                            Ok(policy)
                        });
                let retry = self.retry;
                let (policy, retry) = ctx
                    .run(move || async move { Ok(Json((selected.map_err(invalid)?, retry))) })
                    .name("pin-product-policy")
                    .await?
                    .into_inner();
                let (mut resolved, mut bindings) = self
                    .resolve(&ctx, &command.institution, subject_ref, evidence)
                    .await?
                    .map_err(evidence_error)?;
                bindings.for_policy(&policy);
                use_authorized_address(&mut resolved, &bindings, &policy);
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
                let (mut resolved, mut bindings) = self
                    .resolve(
                        &ctx,
                        &stored.case.tenant_id,
                        &stored.case.subject_ref,
                        evidence,
                    )
                    .await?
                    .map_err(evidence_error)?;
                bindings.for_policy(&stored.case.policy);
                use_authorized_address(&mut resolved, &bindings, &stored.case.policy);
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
                    Action::Review(review)
                        if review.check == ekyc_domain::CheckKind::Sanctions
                            && stored
                                .case
                                .screen_obligations
                                .contains_key(&review.operation_id) =>
                    {
                        stored.case.review_original(
                            &review.operation_id,
                            review.disposition,
                            &command.caller,
                            &review.reason,
                            &review.review_ref,
                            now,
                        )
                    }
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

fn use_authorized_address(
    resolved: &mut super::EvidenceMap,
    bindings: &ekyc_domain::product::BindingSnapshot,
    policy: &ekyc_domain::product::Policy,
) {
    let kind = ekyc_domain::CheckKind::Address;
    if let (Some(asset), Some(routing), Some(candidates)) = (
        resolved.get_mut(&kind),
        policy
            .checks
            .iter()
            .find(|r| r.kind == kind)
            .and_then(|r| r.routing.as_ref()),
        bindings.provider_candidates.get(&kind),
    ) {
        if let Some(approved) = candidates.iter().find(|b| routing.approves(b)) {
            *asset = approved.clone();
        }
    }
}

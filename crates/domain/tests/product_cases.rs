use chrono::{DateTime, Duration, Utc};
use ekyc_domain::product::*;
use ekyc_domain::{CheckKind, Observation};
use std::collections::BTreeMap;

fn now() -> DateTime<Utc> {
    "2026-11-15T12:00:00Z".parse().unwrap()
}

fn evidence(kind: CheckKind) -> BoundEvidence {
    let method = match kind {
        CheckKind::Document => "document_verification",
        CheckKind::Face => "face_comparison",
        CheckKind::Address => "address_verification",
        CheckKind::Sanctions => "sanctions_screening",
    };
    BoundEvidence {
        tenant_id: "bank".into(),
        subject_ref: "customer".into(),
        kind,
        evidence_ref: format!("asset-{}", kind.as_str()),
        revision: "v1".into(),
        provider: kind.as_str().into(),
        callback: false,
        provider_ref: "vendor-bound-reference".into(),
        method: method.into(),
        document_revision: (kind != CheckKind::Address).then(|| "doc1".into()),
        portrait_revision: matches!(kind, CheckKind::Face | CheckKind::Document)
            .then(|| "portrait1".into()),
    }
}
fn bindings() -> BindingSnapshot {
    BindingSnapshot {
        provider_candidates: Default::default(),
        current_document_revision: Some("doc1".into()),
        current_portrait_revision: Some("portrait1".into()),
        screening: Some(evidence(CheckKind::Sanctions)),
    }
}
fn open(product: &str, input: &[CheckKind], candidates: &[Fact], at: DateTime<Utc>) -> ProductCase {
    let use_case = if product == "wallet" {
        "onboarding"
    } else {
        "loan_application"
    };
    ProductCase::open(
        format!("case-{product}"),
        "bank".into(),
        "customer".into(),
        Policy::load(product, use_case).unwrap(),
        input.iter().map(|kind| (*kind, evidence(*kind))).collect(),
        candidates,
        bindings(),
        at,
    )
    .unwrap()
}
fn record(c: &mut ProductCase, kind: CheckKind, obs: Observation, at: DateTime<Utc>) {
    let op = c.checks[&kind].operation_id.clone().unwrap();
    c.record(kind, &op, obs, at, at).unwrap()
}
fn good(kind: CheckKind) -> Observation {
    match kind {
        CheckKind::Document => Observation::Document {
            passed: true,
            confidence: 96.0,
            expires_on: now().date_naive() + Duration::days(365),
        },
        CheckKind::Face => Observation::Face {
            passed: true,
            confidence: 97.0,
            similarity: 98.0,
        },
        CheckKind::Address => Observation::Address {
            passed: true,
            confidence: 98.0,
            proof_date: now().date_naive(),
        },
        CheckKind::Sanctions => Observation::Sanctions { hit: false },
    }
}
fn wallet_facts() -> Vec<Fact> {
    use CheckKind::*;
    let mut c = open("wallet", &[Document, Face], &[], now());
    assert!(c.checks[&Sanctions].operation_id.is_none());
    for kind in [Document, Face, Sanctions] {
        record(&mut c, kind, good(kind), now());
    }
    assert_eq!(c.state, CaseState::Approved);
    c.fresh_passes()
}

#[test]
fn wallet_to_loan_uses_original_checks_and_requests_fresh_address_and_screening() {
    use CheckKind::*;
    let facts = wallet_facts();
    let at = now() + Duration::days(89);
    let mut loan = open("lending", &[], &facts, at);
    assert_eq!(loan.state, CaseState::AwaitingEvidence);
    for kind in [Document, Face] {
        let c = &loan.checks[&kind];
        assert_eq!(c.origin, Some(Origin::Reused));
        let f = c.fact.as_ref().unwrap();
        assert_eq!(f.origin_case_id, "case-wallet");
        assert_eq!(f.observed_at, now());
    }
    assert_eq!(loan.checks[&Sanctions].origin, Some(Origin::Fresh));
    loan.provide(evidence(Address), at).unwrap();
    record(&mut loan, Address, good(Address), at);
    record(&mut loan, Sanctions, good(Sanctions), at);
    assert_eq!(loan.state, CaseState::Approved);
    assert!(loan
        .fresh_passes()
        .iter()
        .all(|fact| fact.origin_case_id == "case-lending"));
    assert_eq!(loan.view()["checks"][0]["origin"], "reused");
}

#[test]
fn blocked_attempt_retries_separately_and_only_live_sanctions_candidate_can_be_adjudicated() {
    use CheckKind::*;
    let mut c = open("wallet", &[Document, Face], &[], now());
    record(&mut c, Document, good(Document), now());
    record(&mut c, Face, good(Face), now());
    let first = c.checks[&Sanctions].operation_id.clone().unwrap();
    record(&mut c, Sanctions, Observation::Unavailable, now());
    assert_eq!(c.state, CaseState::ProviderBlocked);
    assert!(c
        .review(
            Sanctions,
            ReviewAction::Clear,
            "staff",
            "why",
            "case-file",
            now()
        )
        .is_err());
    c.retry(Sanctions, now()).unwrap();
    assert_ne!(c.checks[&Sanctions].operation_id.as_ref().unwrap(), &first);
    c.record(Sanctions, &first, good(Sanctions), now(), now())
        .unwrap();
    record(
        &mut c,
        Sanctions,
        Observation::Sanctions { hit: true },
        now(),
    );
    assert_eq!(c.state, CaseState::AwaitingReview);
    let mut confirmed = c.clone();
    confirmed
        .review(
            Sanctions,
            ReviewAction::Confirm,
            "staff",
            "matched DOB",
            "file",
            now(),
        )
        .unwrap();
    assert_eq!(confirmed.state, CaseState::Rejected);
    c.review(
        Sanctions,
        ReviewAction::Clear,
        "staff",
        "different DOB",
        "file",
        now(),
    )
    .unwrap();
    assert_eq!(c.state, CaseState::Approved);
    assert!(c.checks[&Sanctions]
        .fact
        .as_ref()
        .is_some_and(|f| matches!(f.observation, Observation::Sanctions { hit: true })));
}

#[test]
fn source_age_is_checked_at_finalization_and_new_identity_invalidates_dependencies() {
    use CheckKind::*;
    let facts = wallet_facts();
    let at = now() + Duration::days(90);
    let mut loan = open("lending", &[Address], &facts, at);
    assert_eq!(loan.checks[&Face].origin, Some(Origin::Reused));
    record(
        &mut loan,
        Sanctions,
        good(Sanctions),
        at + Duration::milliseconds(1),
    );
    assert_eq!(loan.checks[&Face].status, CheckStatus::AwaitingEvidence);
    assert_eq!(loan.checks[&Document].status, CheckStatus::AwaitingEvidence);
    let mut loan = open("lending", &[], &facts, now());
    let mut changed = bindings();
    changed.current_portrait_revision = Some("new-portrait".into());
    loan.refresh_bindings(changed, now()).unwrap();
    assert_eq!(loan.checks[&Face].status, CheckStatus::AwaitingEvidence);
}

#[test]
fn mismatched_subject_or_client_supplied_screen_is_rejected() {
    use CheckKind::*;
    let mut doc = evidence(Document);
    doc.subject_ref = "other".into();
    for input in [
        BTreeMap::from([(Document, doc)]),
        BTreeMap::from([(Sanctions, evidence(Sanctions))]),
    ] {
        assert!(ProductCase::open(
            "case".into(),
            "bank".into(),
            "customer".into(),
            Policy::load("wallet", "onboarding").unwrap(),
            input,
            &[],
            bindings(),
            now()
        )
        .is_err());
    }
}

#[test]
fn screening_waiting_for_an_already_supplied_document_needs_no_customer_input() {
    let case = open(
        "wallet",
        &[CheckKind::Document, CheckKind::Face],
        &[],
        now(),
    );
    assert_eq!(case.state, CaseState::Running);
    assert_eq!(case.view()["next_action"], "wait_for_provider");
}

#[test]
fn policy_is_persisted_and_new_products_are_selected_by_config() {
    let mut definition =
        serde_json::to_value(Policy::load("wallet", "onboarding").unwrap()).unwrap();
    definition["product"] = "savings".into();
    definition["use_case"] = "open_account".into();
    definition["version"] = "savings.open_account.v3".into();
    let mut policy = Policy::from_json(&definition.to_string()).unwrap();
    let mut c = ProductCase::open(
        "test-case".into(),
        "bank".into(),
        "customer".into(),
        policy.clone(),
        BTreeMap::new(),
        &[],
        bindings(),
        now(),
    )
    .unwrap();
    policy.checks.clear();
    assert_eq!(c.policy.checks.len(), 3);
    let snapshot = serde_json::to_string(&c).unwrap();
    c = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(c.view()["policy_version"], "savings.open_account.v3");
    definition["checks"][0]["unknown_option"] = true.into();
    assert!(Policy::from_json(&definition.to_string()).is_err());
}

#[test]
fn validation_of_every_reuse_dimension_and_stricter_scores_prevents_false_approvals() {
    use CheckKind::*;
    let original = wallet_facts()
        .into_iter()
        .find(|f| f.evidence.kind == Face)
        .unwrap();
    let mut variants = Vec::new();
    let mut candidate = original.clone();
    candidate.evidence.subject_ref = "other".into();
    variants.push(candidate);
    let mut candidate = original.clone();
    candidate.evidence.tenant_id = "other".into();
    variants.push(candidate);
    let mut candidate = original.clone();
    candidate.source_product = "other".into();
    variants.push(candidate);
    let mut candidate = original.clone();
    candidate.evidence.method = "different".into();
    variants.push(candidate);
    let mut candidate = original.clone();
    candidate.observed_at = now() + Duration::seconds(1);
    variants.push(candidate);
    let mut candidate = original.clone();
    candidate.evidence.document_revision = Some("replaced".into());
    variants.push(candidate);
    let mut candidate = original.clone();
    candidate.evidence.portrait_revision = Some("other".into());
    variants.push(candidate);
    let mut candidate = original.clone();
    candidate.observed_at = now() - Duration::days(91);
    variants.push(candidate);
    for fact in variants {
        let c = open("lending", &[], &[fact], now());
        assert_eq!(c.checks[&Face].status, CheckStatus::AwaitingEvidence);
    }
    let mut policy = Policy::load("lending", "loan_application").unwrap();
    policy
        .checks
        .iter_mut()
        .find(|r| r.kind == Face)
        .unwrap()
        .min_similarity = Some(99.0);
    let c = ProductCase::open(
        "loan".into(),
        "bank".into(),
        "customer".into(),
        policy,
        BTreeMap::new(),
        &[original],
        bindings(),
        now(),
    )
    .unwrap();
    assert_eq!(c.checks[&Face].status, CheckStatus::AwaitingEvidence);
}

#[test]
fn replacing_customer_correctable_evidence_creates_new_operation_and_keeps_completed_case_immutable(
) {
    use CheckKind::*;
    let mut c = open("wallet", &[Document, Face], &[], now());
    let old = c.checks[&Document].operation_id.clone().unwrap();
    record(
        &mut c,
        Document,
        Observation::Document {
            passed: true,
            confidence: 95.0,
            expires_on: now().date_naive() - Duration::days(1),
        },
        now(),
    );
    assert_eq!(c.state, CaseState::AwaitingEvidence);
    assert!(c.provide(evidence(Document), now()).is_err());
    let mut replacement = evidence(Document);
    replacement.revision = "new-evidence".into();
    c.provide(replacement, now()).unwrap();
    assert_ne!(c.checks[&Document].operation_id.as_ref().unwrap(), &old);
    c.record(Document, &old, good(Document), now(), now())
        .unwrap();
    assert_eq!(c.checks[&Document].status, CheckStatus::Pending);
    for kind in [Document, Face, Sanctions] {
        record(&mut c, kind, good(kind), now());
    }
    let terminal = c.clone();
    c.refresh_bindings(BindingSnapshot::default(), now() + Duration::days(500))
        .unwrap();
    c.record(
        Sanctions,
        "old",
        Observation::Sanctions { hit: true },
        now(),
        now(),
    )
    .unwrap();
    assert_eq!(c, terminal);
}

#[test]
fn retry_uses_current_provider_reference_for_the_same_verified_identity() {
    use CheckKind::*;
    let mut c = open("wallet", &[Document, Face], &[], now());
    record(&mut c, Document, good(Document), now());
    record(&mut c, Sanctions, Observation::Unavailable, now());
    let mut updated = bindings();
    updated.screening.as_mut().unwrap().provider_ref = "ref-after-outage".into();
    c.refresh_bindings(updated, now()).unwrap();
    c.retry(Sanctions, now()).unwrap();
    assert_eq!(c.checks[&Sanctions].attempt, 2);
    assert_eq!(
        c.checks[&Sanctions].evidence.as_ref().unwrap().provider_ref,
        "ref-after-outage"
    );
}

#[test]
fn newer_substantive_findings_supersede_prior_approvals_even_if_loan_threshold_would_pass() {
    use CheckKind::*;
    let at = now() + Duration::hours(1);
    let original = wallet_facts();
    let mut source = ProductCase::open(
        "another-wallet".into(),
        "bank".into(),
        "customer".into(),
        Policy::load("wallet", "onboarding").unwrap(),
        BTreeMap::from([(Face, evidence(Face))]),
        &[],
        bindings(),
        at,
    )
    .unwrap();
    record(
        &mut source,
        Face,
        Observation::Face {
            passed: true,
            confidence: 83.0,
            similarity: 95.0,
        },
        at,
    );
    let mut all = original.clone();
    all.extend(source.fresh_adverse());
    assert_eq!(all.len(), original.len() + 1);
    let mut loan = open("lending", &[], &original, now());
    loan.revalidate_reuse(&all, at);
    assert_eq!(loan.checks[&Face].status, CheckStatus::AwaitingEvidence);
    assert_eq!(loan.checks[&Document].origin, Some(Origin::Reused));
    let mut permissive = Policy::load("lending", "loan_application").unwrap();
    permissive
        .checks
        .iter_mut()
        .find(|r| r.kind == Face)
        .unwrap()
        .min_confidence = Some(80.0);
    let loan = ProductCase::open(
        "permissive".into(),
        "bank".into(),
        "customer".into(),
        permissive,
        BTreeMap::new(),
        &all,
        bindings(),
        at,
    )
    .unwrap();
    assert_eq!(loan.checks[&Face].status, CheckStatus::AwaitingEvidence);
    let mut blocked = open("wallet", &[Document], &[], at);
    record(&mut blocked, Document, Observation::Unavailable, at);
    assert!(blocked.fresh_adverse().is_empty());
}

#[test]
fn same_timestamp_adverse_cannot_lose_to_a_pass_through_operation_id_ordering() {
    use CheckKind::*;
    let mut facts = wallet_facts();
    let mut adverse = facts
        .iter()
        .find(|f| f.evidence.kind == Document)
        .unwrap()
        .clone();
    adverse.source_passed = false;
    adverse.operation_id = "a-before-passing-id".into();
    adverse.observation = Observation::Document {
        passed: false,
        confidence: 99.0,
        expires_on: now().date_naive(),
    };
    facts.push(adverse);
    let loan = open("lending", &[], &facts, now());
    assert_eq!(loan.checks[&Document].status, CheckStatus::AwaitingEvidence);
}

#[test]
fn a_new_pass_does_not_rehabilitate_the_older_fact_pinned_before_an_intervening_adverse_check() {
    use CheckKind::*;
    let initial = wallet_facts();
    let mut loan = open("lending", &[], &initial, now());
    let mut findings = initial.clone();
    let mut later = initial
        .iter()
        .find(|f| f.evidence.kind == Face)
        .unwrap()
        .clone();
    later.source_passed = false;
    later.operation_id = "another-wallet/face/1".into();
    later.origin_case_id = "another-wallet".into();
    later.observed_at = now() + Duration::hours(1);
    findings.push(later.clone());
    later.source_passed = true;
    later.operation_id = "third-wallet/face/1".into();
    later.origin_case_id = "third-wallet".into();
    later.observed_at = now() + Duration::hours(2);
    findings.push(later);
    loan.revalidate_reuse(&findings, now() + Duration::hours(3));
    assert_eq!(loan.checks[&Face].status, CheckStatus::AwaitingEvidence);
    let opened_later = open("lending", &[], &findings, now() + Duration::hours(3));
    assert_eq!(
        opened_later.checks[&Face]
            .fact
            .as_ref()
            .unwrap()
            .origin_case_id,
        "third-wallet"
    );
}

#[test]
fn a_previously_unavailable_identity_can_be_retried_once_the_issuer_restores_it() {
    use CheckKind::*;
    let mut snapshot = bindings();
    snapshot.screening = None;
    let mut c = ProductCase::open(
        "wallet-missing-identity".into(),
        "bank".into(),
        "customer".into(),
        Policy::load("wallet", "onboarding").unwrap(),
        BTreeMap::from([(Document, evidence(Document)), (Face, evidence(Face))]),
        &[],
        snapshot,
        now(),
    )
    .unwrap();
    record(&mut c, Document, good(Document), now());
    record(&mut c, Face, good(Face), now());
    assert_eq!(c.state, CaseState::ProviderBlocked);
    c.refresh_bindings(bindings(), now()).unwrap();
    c.retry(Sanctions, now()).unwrap();
    assert_eq!(c.checks[&Sanctions].attempt, 1);
}

#[test]
fn unvalidated_provider_result_blocks_the_current_attempt_without_overwriting_a_later_attempt() {
    let mut c = open(
        "wallet",
        &[CheckKind::Document, CheckKind::Face],
        &[],
        now(),
    );
    let op = c.checks[&CheckKind::Document].operation_id.clone().unwrap();
    c.binding_failure(
        CheckKind::Document,
        &op,
        now(),
        Some(now()),
        now(),
        "EVIDENCE_REGISTRY_UNAVAILABLE",
    )
    .unwrap();
    assert_eq!(c.state, CaseState::ProviderBlocked);
    let check = &c.checks[&CheckKind::Document];
    assert_eq!(check.reasons, ["EVIDENCE_REGISTRY_UNAVAILABLE"]);
    assert_eq!(check.fact.as_ref().unwrap().operation_id, op);
    c.retry(CheckKind::Document, now()).unwrap();
    let snapshot = c.clone();
    c.binding_failure(
        CheckKind::Document,
        &op,
        now(),
        Some(now()),
        now(),
        "INVALID_EVIDENCE_BINDING",
    )
    .unwrap();
    assert_eq!(c, snapshot);
}

#[test]
fn original_document_expiry_is_checked_on_the_target_decision_utc_date() {
    use CheckKind::*;
    let mut facts = wallet_facts();
    let expiry = now().date_naive();
    let document = facts
        .iter_mut()
        .find(|f| f.evidence.kind == Document)
        .unwrap();
    document.observation = Observation::Document {
        passed: true,
        confidence: 95.0,
        expires_on: expiry,
    };
    let mut loan = open("lending", &[Address], &facts, now());
    assert_eq!(loan.checks[&Document].origin, Some(Origin::Reused));
    record(&mut loan, Address, good(Address), now());
    let following_day = DateTime::from_naive_utc_and_offset(
        (expiry + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap(),
        Utc,
    );
    record(&mut loan, Sanctions, good(Sanctions), following_day);
    assert_eq!(loan.state, CaseState::AwaitingEvidence);
    assert_eq!(loan.checks[&Document].reasons, ["DOCUMENT_EXPIRED"]);
}

#[test]
fn loan_address_age_uses_the_proof_date_including_the_exact_ninetieth_day() {
    use CheckKind::*;
    let facts = wallet_facts();
    for (age, expected) in [
        (90, CheckStatus::Passed),
        (91, CheckStatus::AwaitingEvidence),
        (-1, CheckStatus::AwaitingEvidence),
    ] {
        let mut c = open("lending", &[Address], &facts, now());
        record(
            &mut c,
            Address,
            Observation::Address {
                passed: true,
                confidence: 95.0,
                proof_date: now().date_naive() - Duration::days(age),
            },
            now(),
        );
        assert_eq!(c.checks[&Address].status, expected);
    }
}

#[test]
fn loan_can_be_entirely_fresh_with_all_or_staged_inputs_and_no_wallet() {
    use CheckKind::*;
    for stage in [false, true] {
        let mut c = open(
            "lending",
            if stage {
                &[]
            } else {
                &[Document, Face, Address]
            },
            &[],
            now(),
        );
        if stage {
            assert_eq!(c.state, CaseState::AwaitingEvidence);
            for kind in [Address, Document, Face] {
                c.provide(evidence(kind), now()).unwrap();
            }
        }
        for kind in [Face, Address, Document, Sanctions] {
            record(&mut c, kind, good(kind), now());
        }
        assert_eq!(c.state, CaseState::Approved);
        assert!(c.checks.values().all(|c| c.origin == Some(Origin::Fresh)));
    }
}

#[test]
fn address_refresh_scopes_and_completion_use_only_the_address_and_never_reuse_it() {
    use CheckKind::*;
    let policy = Policy::load("wallet", "address_refresh").unwrap();
    let mut c = ProductCase::open(
        "refresh".into(),
        "bank".into(),
        "customer".into(),
        policy.clone(),
        BTreeMap::new(),
        &wallet_facts(),
        BindingSnapshot::default(),
        now(),
    )
    .unwrap();
    assert_eq!(c.checks.keys().copied().collect::<Vec<_>>(), vec![Address]);
    assert!(c.provide(evidence(Document), now()).is_err());
    c.provide(evidence(Address), now()).unwrap();
    record(&mut c, Address, good(Address), now());
    assert_eq!(c.state, CaseState::Approved);
    assert_eq!(c.checks[&Address].origin, Some(Origin::Fresh));
}

#[test]
fn interactive_status_is_scoped_to_the_current_fresh_provider_check() {
    use CheckKind::*;
    let mut doc = evidence(Document);
    doc.callback = true;
    let mut wallet = ProductCase::open(
        "wallet-live".into(),
        "bank".into(),
        "customer".into(),
        Policy::load("wallet", "onboarding").unwrap(),
        BTreeMap::from([(Document, doc), (Face, evidence(Face))]),
        &[],
        bindings(),
        now(),
    )
    .unwrap();
    assert_eq!(wallet.view()["checks"][0]["interactive"], true);
    for kind in [Document, Face, Sanctions] {
        record(&mut wallet, kind, good(kind), now());
    }
    let loan = open("lending", &[], &wallet.fresh_passes(), now());
    assert_eq!(loan.view()["checks"][0]["interactive"], false);
}

#[test]
fn product_scores_use_strict_thresholds_and_malformed_vendor_results_block() {
    use CheckKind::*;
    let mut c = open("lending", &[Document, Face, Address], &[], now());
    record(
        &mut c,
        Document,
        Observation::Document {
            passed: true,
            confidence: 85.0,
            expires_on: now().date_naive(),
        },
        now(),
    );
    record(
        &mut c,
        Face,
        Observation::Face {
            passed: true,
            confidence: 90.0,
            similarity: 85.0,
        },
        now(),
    );
    record(
        &mut c,
        Address,
        Observation::Address {
            passed: true,
            confidence: 80.0,
            proof_date: now().date_naive(),
        },
        now(),
    );
    for (kind, reason) in [
        (Document, "DOCUMENT_CONFIDENCE_LOW"),
        (Face, "FACE_SIMILARITY_LOW"),
        (Address, "ADDRESS_CONFIDENCE_LOW"),
    ] {
        assert_eq!(c.checks[&kind].status, CheckStatus::AwaitingEvidence);
        assert_eq!(c.checks[&kind].reasons, [reason]);
    }
    let mut c = open("lending", &[Document, Face], &[], now());
    record(&mut c, Document, good(Address), now());
    record(
        &mut c,
        Face,
        Observation::Face {
            passed: true,
            confidence: 110.0,
            similarity: 87.0,
        },
        now(),
    );
    assert_eq!(c.checks[&Document].status, CheckStatus::Blocked);
    assert_eq!(c.checks[&Face].reasons, ["INVALID_SCORE"]);
}

#[test]
fn full_wallet_onboarding_requires_fresh_address_and_all_other_checks() {
    use CheckKind::*;
    let mut c = ProductCase::open(
        "wallet-full".into(),
        "bank".into(),
        "customer".into(),
        Policy::load("wallet", "full_onboarding").unwrap(),
        BTreeMap::from([(Document, evidence(Document)), (Face, evidence(Face))]),
        &wallet_facts(),
        bindings(),
        now(),
    )
    .unwrap();
    assert_eq!(c.checks.len(), 4);
    for kind in [Document, Face, Sanctions] {
        record(&mut c, kind, good(kind), now());
    }
    assert_eq!(c.state, CaseState::AwaitingEvidence);
    c.provide(evidence(Address), now()).unwrap();
    record(&mut c, Address, good(Address), now());
    assert_eq!(c.state, CaseState::Approved);
    assert!(c.checks.values().all(|r| r.origin == Some(Origin::Fresh)));
}

#[test]
fn a_single_eligible_wallet_fact_only_replaces_that_specific_loan_check() {
    use CheckKind::*;
    let originals: Vec<_> = wallet_facts()
        .into_iter()
        .filter(|f| f.evidence.kind == Document)
        .collect();
    let mut c = open("lending", &[Address], &originals, now());
    assert_eq!(c.checks[&Document].origin, Some(Origin::Reused));
    assert_eq!(c.checks[&Face].status, CheckStatus::AwaitingEvidence);
    c.provide(evidence(Face), now()).unwrap();
    for kind in [Face, Address, Sanctions] {
        record(&mut c, kind, good(kind), now());
    }
    assert_eq!(c.state, CaseState::Approved);
    assert_eq!(c.checks[&Face].origin, Some(Origin::Fresh));
}

#[test]
fn directory_state_filters_match_every_serialized_case_state() {
    for state in CaseState::ALL {
        assert_eq!(serde_json::to_value(state).unwrap(), state.as_str());
    }
}

#[test]
fn a_future_timestamp_from_a_provider_does_not_leave_its_operation_pending() {
    use CheckKind::*;
    let mut c = open("wallet", &[Document, Face], &[], now());
    record(&mut c, Document, good(Document), now());
    let op = c.checks[&Sanctions].operation_id.clone().unwrap();
    c.record(
        Sanctions,
        &op,
        Observation::Sanctions { hit: true },
        now() + Duration::seconds(1),
        now(),
    )
    .unwrap();
    assert_eq!(c.checks[&Sanctions].status, CheckStatus::Blocked);
    assert_eq!(c.checks[&Sanctions].reasons, ["OBSERVATION_TIME_INVALID"]);
    assert!(matches!(
        c.checks[&Sanctions].fact.as_ref().unwrap().observation,
        Observation::Sanctions { hit: true }
    ));
}

#[test]
fn policy_product_and_use_case_must_be_representable_by_api_grants() {
    let mut policy = Policy::load("wallet", "onboarding").unwrap();
    policy.product = "new_product-2".into();
    assert!(policy.validate().is_ok());
    policy.use_case = "ungrantable purpose".into();
    assert!(policy.validate().is_err());
}

#[test]
fn reviewer_notes_support_multiline_and_count_unicode_characters() {
    use CheckKind::*;
    let mut case = open("wallet", &[Document, Face], &[], now());
    record(&mut case, Document, good(Document), now());
    record(&mut case, Face, good(Face), now());
    record(
        &mut case,
        Sanctions,
        Observation::Sanctions { hit: true },
        now(),
    );
    for reason in ["ي".repeat(512), "First line\nتأكيد".to_string()] {
        let mut pending = case.clone();
        pending
            .review(
                Sanctions,
                ReviewAction::Clear,
                "reviewer",
                &reason,
                "file",
                now(),
            )
            .unwrap();
        assert_eq!(pending.reviews[0].reason, reason);
    }
    for reason in ["ي".repeat(513), "line\t2".to_string(), "\n ".to_string()] {
        let mut pending = case.clone();
        assert!(pending
            .review(
                Sanctions,
                ReviewAction::Clear,
                "reviewer",
                &reason,
                "file",
                now()
            )
            .is_err());
    }
}

#[test]
fn public_projection_and_command_preserve_v2_wire_format() {
    use ekyc_domain::product::{wire::Command, PublicCase};
    let c = open("wallet", &[], &[], now());
    let projection: PublicCase = serde_json::from_value(c.view()).unwrap();
    assert_eq!(projection, c.public_view());
    assert_eq!(serde_json::to_value(&projection).unwrap(), c.view());
    // Historical v2 revisions were persisted before vendor receipt times were exposed.
    let mut historical_view = c.view();
    for check in historical_view["checks"].as_array_mut().unwrap() {
        check.as_object_mut().unwrap().remove("received_at");
    }
    let historical: PublicCase = serde_json::from_value(historical_view).unwrap();
    assert_eq!(historical, projection);
    let persisted = serde_json::json!({"caller":"wallet", "institution":"one", "product":"wallet","key":"one",
        "action":{"type":"retry","check":"face"}});
    let receipt: Command = serde_json::from_value(persisted.clone()).unwrap();
    assert_eq!(receipt.receipt_key(), "command/wallet/one");
    assert_eq!(serde_json::to_value(receipt).unwrap(), persisted);
}

#[test]
fn delayed_old_pass_cannot_displace_newer_adverse_evaluation() {
    use CheckKind::*;
    let mut source = open("wallet", &[Document, Face], &[], now());
    let received = now() + Duration::minutes(10);
    let op = source.checks[&Face].operation_id.clone().unwrap();
    source
        .record_received(Face, &op, good(Face), now(), Some(received), received)
        .unwrap();
    record(&mut source, Document, good(Document), received);
    record(&mut source, Sanctions, good(Sanctions), received);
    let mut facts = source.fresh_passes();
    let old = facts
        .iter()
        .find(|f| f.evidence.kind == Face)
        .unwrap()
        .clone();
    assert_eq!((old.observed_at, old.received_at), (now(), Some(received)));
    let mut failed = old.clone();
    failed.operation_id = "other-wallet/face/1".into();
    failed.source_passed = false;
    failed.observed_at = now() + Duration::minutes(5);
    failed.received_at = Some(failed.observed_at);
    failed.observation = Observation::Face {
        passed: false,
        confidence: 99.0,
        similarity: 60.0,
    };
    facts.push(failed);
    let lending = open("lending", &[], &facts, received);
    assert_eq!(lending.checks[&Face].status, CheckStatus::AwaitingEvidence);

    let unordered: Vec<_> = facts
        .into_iter()
        .filter(|f| f.source_passed)
        .map(|mut f| {
            f.received_at = None;
            f
        })
        .collect();
    let legacy = open("lending", &[], &unordered, received);
    assert_eq!(legacy.checks[&Face].origin, None);
}

#[test]
fn strict_screening_policy_is_separately_selectable() {
    let policy = Policy::load("lending", "strict_screening").expect("strict screening pilot");
    assert_ne!(
        policy.version,
        Policy::load("lending", "loan_application").unwrap().version
    );
}

fn strict_case() -> ProductCase {
    let policy = Policy::load("lending", "strict_screening").unwrap();
    let mut snapshot = serde_json::to_value(bindings()).unwrap();
    let mut a = evidence(CheckKind::Sanctions);
    a.provider = "sanctions-a".into();
    let mut b = a.clone();
    b.provider = "sanctions-b".into();
    b.provider_ref = "b-authorized-locator".into();
    snapshot["provider_candidates"] = serde_json::json!({"sanctions": [a,b]});
    let mut case = ProductCase::open(
        "strict-1".into(),
        "bank".into(),
        "customer".into(),
        policy,
        [(CheckKind::Document, evidence(CheckKind::Document))].into(),
        &[],
        serde_json::from_value(snapshot).unwrap(),
        now(),
    )
    .unwrap();
    record(
        &mut case,
        CheckKind::Document,
        good(CheckKind::Document),
        now(),
    );
    case
}

#[test]
fn strict_replacement_clear_remains_held_until_original_resolves() {
    let mut case = strict_case();
    let original = case.checks[&CheckKind::Sanctions]
        .operation_id
        .clone()
        .unwrap();
    record(
        &mut case,
        CheckKind::Sanctions,
        Observation::Unavailable,
        now(),
    );
    assert_eq!(
        case.checks[&CheckKind::Sanctions]
            .evidence
            .as_ref()
            .unwrap()
            .provider,
        "sanctions-b"
    );
    record(
        &mut case,
        CheckKind::Sanctions,
        good(CheckKind::Sanctions),
        now(),
    );
    assert_eq!(case.state, CaseState::ProviderBlocked);
    let mut hit = case.clone();
    hit.record_screen_lookup(&original, ScreenOutcome::Hit, now(), now(), now())
        .unwrap();
    assert_eq!(hit.state, CaseState::AwaitingReview);
    let prior = hit.clone();
    hit.record_screen_lookup(&original, ScreenOutcome::Hit, now(), now(), now())
        .unwrap();
    assert_eq!(hit, prior);
    hit.review_original(
        &original,
        ReviewAction::Confirm,
        "reviewer",
        "confirmed identity",
        "file-1",
        now(),
    )
    .unwrap();
    assert_eq!(hit.state, CaseState::Rejected);
    for result in [ScreenOutcome::Clear, ScreenOutcome::NotAccepted] {
        let mut resolved = case.clone();
        resolved
            .record_screen_lookup(&original, result, now(), now(), now())
            .unwrap();
        assert_eq!(resolved.state, CaseState::Approved);
    }
    case.record_screen_lookup(&original, ScreenOutcome::Inconclusive, now(), now(), now())
        .unwrap();
    assert_eq!(case.state, CaseState::AwaitingReview);
    case.review_original(
        &original,
        ReviewAction::Clear,
        "reviewer",
        "resolved inconclusive screen",
        "file-2",
        now(),
    )
    .unwrap();
    assert_eq!(case.state, CaseState::Approved);
}

#[test]
fn strict_both_ambiguous_reconciles_each_vendor_after_submission_expires() {
    let mut case = strict_case();
    let a = case.checks[&CheckKind::Sanctions]
        .operation_id
        .clone()
        .unwrap();
    record(
        &mut case,
        CheckKind::Sanctions,
        Observation::Unavailable,
        now(),
    );
    let b = case.checks[&CheckKind::Sanctions]
        .operation_id
        .clone()
        .unwrap();
    record(
        &mut case,
        CheckKind::Sanctions,
        Observation::Unavailable,
        now(),
    );
    let late = now() + Duration::hours(1);
    for result in [
        ScreenOutcome::Clear,
        ScreenOutcome::Hit,
        ScreenOutcome::Inconclusive,
        ScreenOutcome::NotAccepted,
    ] {
        let mut found = case.clone();
        found
            .record_screen_lookup(&b, result, late, late, late)
            .unwrap();
        assert_ne!(found.state, CaseState::Approved);
        found
            .record_screen_lookup(&a, ScreenOutcome::Clear, late, late, late)
            .unwrap();
        match result {
            ScreenOutcome::Hit | ScreenOutcome::Inconclusive => {
                assert_eq!(found.state, CaseState::AwaitingReview);
                found
                    .review_original(
                        &b,
                        ReviewAction::Confirm,
                        "reviewer",
                        "confirmed replacement",
                        "b-review",
                        late,
                    )
                    .unwrap();
                assert_eq!(found.state, CaseState::Rejected);
            }
            _ => assert_eq!(found.state, CaseState::Approved),
        }
    }
}

#[test]
fn fenced_unsent_original_allows_linked_verified_replacement() {
    let mut case = strict_case();
    let original = case.checks[&CheckKind::Sanctions]
        .operation_id
        .clone()
        .unwrap();
    case.record_screen_lookup(&original, ScreenOutcome::NotAccepted, now(), now(), now())
        .unwrap();
    let b = case.checks[&CheckKind::Sanctions]
        .operation_id
        .clone()
        .unwrap();
    assert_eq!(
        case.screen_obligations[&original]
            .replacement_operation
            .as_ref(),
        Some(&b)
    );
    assert_eq!(case.state, CaseState::Running);
    case.record_screen_lookup(&b, ScreenOutcome::Clear, now(), now(), now())
        .unwrap();
    assert_eq!(case.state, CaseState::Approved);
}

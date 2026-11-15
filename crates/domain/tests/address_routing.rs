use ekyc_domain::product::Policy;
use ekyc_domain::product::{AddressRouting, PlacementMode, RouteAvailability};
use std::collections::BTreeMap;

#[test]
fn all_weighted_buckets_and_priority_have_deterministic_fallbacks() {
    let policy = Policy::load("wallet", "address-pilot").unwrap();
    let routing: &AddressRouting = policy.checks[0].routing.as_ref().unwrap();
    let both = vec!["address-a".to_owned(), "address-b".to_owned()];
    let mut limits = BTreeMap::new();
    for i in 0..100 {
        let route = routing.select(&both, &limits, i).unwrap();
        assert_eq!(
            route.provider,
            if i < 80 { "address-a" } else { "address-b" }
        );
        assert_eq!(route.reason, "weighted");
    }
    limits.insert(
        "address-a".into(),
        RouteAvailability {
            blocked: true,
            ..Default::default()
        },
    );
    assert_eq!(
        routing.select(&both, &limits, 0).unwrap().reason,
        "outage_diversion"
    );
    assert!(routing.select(&both[..1], &limits, 0).is_none());
    limits.clear();
    limits.insert(
        "address-a".into(),
        RouteAvailability {
            share: Some(5),
            ..Default::default()
        },
    );
    assert_eq!(
        routing.select(&both, &limits, 5).unwrap().provider,
        "address-b"
    );
    assert_eq!(
        routing.select(&both, &limits, 4).unwrap().provider,
        "address-a"
    );
    let mut ordered = routing.clone();
    ordered.mode = PlacementMode::Priority;
    assert_eq!(
        ordered
            .select(&both, &BTreeMap::new(), 99)
            .unwrap()
            .provider,
        "address-a"
    );
    assert_eq!(
        ordered
            .select(&both[1..], &BTreeMap::new(), 5)
            .unwrap()
            .reason,
        "single_eligible"
    );
    let count = (0..10_000)
        .filter(|i| routing.bucket(&format!("case-{i}/address/r")) < 80)
        .count();
    assert!((7800..=8200).contains(&count), "{count}");
}

#[test]
fn replacement_clear_waits_for_original_and_conflicting_findings_require_review() {
    use chrono::{DateTime, Duration, Utc};
    use ekyc_domain::{
        product::{
            BindingSnapshot, BoundEvidence, CaseState, ProductCase, ReviewAction, RoutedAnswer,
        },
        CheckKind, Observation,
    };
    let now: DateTime<Utc> = "2026-11-16T10:00:00Z".parse().unwrap();
    let asset = |vendor: &str| BoundEvidence {
        kind: CheckKind::Address,
        tenant_id: "bank".into(),
        subject_ref: "s".into(),
        evidence_ref: "e".into(),
        revision: "v1".into(),
        provider: vendor.into(),
        provider_ref: format!("opaque-{vendor}"),
        callback: false,
        method: format!("address_verification_{}", vendor.chars().last().unwrap()),
        document_revision: None,
        portrait_revision: None,
    };
    let (a, b) = (asset("address-a"), asset("address-b"));
    let mut case = ProductCase::open(
        "case".into(),
        "bank".into(),
        "s".into(),
        Policy::load("wallet", "address-replacement").unwrap(),
        BTreeMap::from([(CheckKind::Address, a.clone())]),
        &[],
        BindingSnapshot {
            provider_candidates: BTreeMap::from([(CheckKind::Address, vec![a.clone(), b.clone()])]),
            ..Default::default()
        },
        now,
    )
    .unwrap();
    assert!(case.replace_address("case/address/1", now));
    case.checks.get_mut(&CheckKind::Address).unwrap().evidence = Some(b.clone());
    let mut answers = vec![
        RoutedAnswer {
            operation_id: "case/address/1".into(),
            status: "outstanding".into(),
            binding: a.clone(),
            observation: None,
            observed_at: None,
            received_at: None,
        },
        RoutedAnswer {
            operation_id: "case/address/2".into(),
            status: "complete".into(),
            binding: b,
            observation: Some(Observation::Address {
                passed: true,
                confidence: 98.0,
                proof_date: now.date_naive(),
            }),
            observed_at: Some(now),
            received_at: Some(now),
        },
    ];
    case.settle_address(&answers, now).unwrap();
    assert_eq!(case.state, CaseState::ProviderBlocked);
    assert!(!case.replace_address("case/address/2", now));
    let mut fenced = case.clone();
    answers[0].status = "not_accepted".into();
    fenced.settle_address(&answers, now).unwrap();
    assert_eq!(fenced.state, CaseState::Approved);
    answers[0].status = "complete".into();
    answers[0].observation = Some(Observation::Address {
        passed: true,
        confidence: 98.0,
        proof_date: (now - Duration::days(100)).date_naive(),
    });
    answers[0].observed_at = Some(now);
    answers[0].received_at = Some(now);
    case.settle_address(&answers, now).unwrap();
    assert_eq!(case.state, CaseState::AwaitingReview);
    let version = case.revision;
    case.settle_address(&answers, now).unwrap();
    assert_eq!(case.revision, version);
    case.review(
        CheckKind::Address,
        ReviewAction::Confirm,
        "reviewer",
        "Conflicting address proof validated by reviewer",
        "review-1",
        now,
    )
    .unwrap();
    assert_eq!(case.state, CaseState::AwaitingEvidence);
}

#[test]
fn vendor_scale_is_pinned_and_checked_for_its_authorized_method() {
    use chrono::{DateTime, Utc};
    use ekyc_domain::{
        product::{BindingSnapshot, BoundEvidence, CaseState, ProductCase},
        CheckKind, Observation,
    };
    let now: DateTime<Utc> = "2026-11-16T10:00:00Z".parse().unwrap();
    let mut policy = Policy::load("wallet", "address-pilot").unwrap();
    policy.checks[0].routing.as_mut().unwrap().roster[1].min_confidence = 99.0;
    let b = BoundEvidence {
        tenant_id: "bank".into(),
        subject_ref: "subject".into(),
        kind: CheckKind::Address,
        evidence_ref: "ref".into(),
        revision: "v1".into(),
        provider: "address-b".into(),
        provider_ref: "locator".into(),
        callback: false,
        method: "address_verification_b".into(),
        document_revision: None,
        portrait_revision: None,
    };
    let mut case = ProductCase::open(
        "case".into(),
        "bank".into(),
        "subject".into(),
        policy,
        BTreeMap::from([(CheckKind::Address, b)]),
        &[],
        BindingSnapshot::default(),
        now,
    )
    .unwrap();
    case.record(
        CheckKind::Address,
        "case/address/1",
        Observation::Address {
            passed: true,
            confidence: 98.0,
            proof_date: now.date_naive(),
        },
        now,
        now,
    )
    .unwrap();
    assert_eq!(case.state, CaseState::AwaitingEvidence);
    assert_eq!(
        case.checks[&CheckKind::Address].reasons,
        ["ADDRESS_CONFIDENCE_LOW"]
    );
}

#[test]
fn custom_policy_cannot_place_unwired_vendors_and_priority_uses_the_same_cap_buckets() {
    let mut policy = Policy::load("wallet", "address-priority").unwrap();
    let routing = policy.checks[0].routing.as_mut().unwrap();
    routing.roster.reverse();
    let eligible = vec!["address-a".into(), "address-b".into()];
    let limits = BTreeMap::from([(
        "address-b".to_owned(),
        RouteAvailability {
            share: Some(20),
            ..Default::default()
        },
    )]);
    assert_eq!(
        routing.select(&eligible, &limits, 90).unwrap().provider,
        "address-b"
    );
    assert_eq!(
        routing.select(&eligible, &limits, 5).unwrap().provider,
        "address-a"
    );
    routing.roster[0].provider = "unwired-provider".into();
    assert!(policy.validate().is_err());
}

#[test]
fn mixed_identity_and_routed_address_policy_requires_an_integrated_settlement_contract() {
    let mut mixed = Policy::load("wallet", "full_onboarding").unwrap();
    let routed = Policy::load("wallet", "address-pilot")
        .unwrap()
        .checks
        .remove(0);
    let address = mixed
        .checks
        .iter_mut()
        .find(|r| r.kind == ekyc_domain::CheckKind::Address)
        .unwrap();
    *address = routed;
    let encoded = serde_json::to_string(&mixed).unwrap();
    assert!(Policy::from_json(&encoded).is_err());
}

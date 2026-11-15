use super::*;
use ekyc_domain::product::Policy;
use serde_json::json;
use std::fs;

#[test]
fn provider_budget_begins_on_input_and_a_pending_operation_dispatches_once() {
    let now = Utc::now();
    let bindings = BindingSnapshot {
        current_document_revision: Some("document-v1".into()),
        current_portrait_revision: Some("portrait-v1".into()),
        screening: None,
    };
    let case = Case::open(
        "wallet-case".into(),
        "bank".into(),
        "customer".into(),
        Policy::load("wallet", "onboarding").unwrap(),
        BTreeMap::new(),
        &[],
        bindings,
        now,
    )
    .unwrap();
    let mut stored = Stored {
        case,
        opener: "wallet".into(),
        retry: RetrySettings {
            case_deadline_ms: 2000,
            max_attempts: 1,
            retry_base_ms: 100,
        },
        flights: BTreeMap::new(),
    };
    assert!(stored.schedule(now).is_empty());
    let supplied_at = now + chrono::Duration::hours(2);
    stored
        .case
        .provide(
            BoundEvidence {
                tenant_id: "bank".into(),
                subject_ref: "customer".into(),
                kind: CheckKind::Document,
                evidence_ref: "issuer-token".into(),
                revision: "document-v1".into(),
                provider: "document".into(),
                callback: false,
                provider_ref: "vendor-asset".into(),
                method: "document_verification".into(),
                document_revision: Some("document-v1".into()),
                portrait_revision: Some("portrait-v1".into()),
            },
            supplied_at,
        )
        .unwrap();
    let jobs = stored.schedule(supplied_at);
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].evidence_ref, "vendor-asset");
    assert_eq!(jobs[0].deadline_ms, supplied_at.timestamp_millis() + 2000);
    assert!(stored
        .schedule(supplied_at + chrono::Duration::seconds(1))
        .is_empty());
}

#[test]
fn one_late_screening_alert_survives_timeout_and_prior_nonpositive_deliveries() {
    let now = Utc::now();
    let positive = ProductResult {
        operation: Operation {
            kind: CheckKind::Sanctions,
            id: "operation-1".into(),
        },
        observation: Observation::Sanctions { hit: true },
        observed_at: now,
        received_at: Some(now),
    };
    assert!(positive.is_positive_screen());
    assert_eq!(
        positive.select(Some(now.timestamp_millis() + 1)),
        Selection::Current
    );
    assert_eq!(
        positive.select(Some(now.timestamp_millis())),
        Selection::TimedOut
    );
    assert_eq!(
        positive.select(Some(now.timestamp_millis() - 1)),
        Selection::TimedOut
    );
    assert_eq!(positive.select(None), Selection::Superseded);
    assert!(!already_received(None, true)); // Timer settles without a supplier receipt.
    assert!(!already_received(Some(false), true)); // Supplier corrects an earlier nonpositive result.
    assert!(already_received(Some(true), true)); // Duplicate positive after review or closure.
    assert!(already_received(Some(false), false));
}

#[test]
fn evidence_can_only_name_the_provider_registered_for_its_kind() {
    let mut snap = EvidenceSnapshot {
        evidence: BTreeMap::from([(
            CheckKind::Address,
            BoundEvidence {
                tenant_id: "one".into(),
                subject_ref: "customer".into(),
                kind: CheckKind::Address,
                revision: "v1".into(),
                evidence_ref: "evidence-1".into(),
                provider: "address".into(),
                callback: false,
                provider_ref: "vendor-asset".into(),
                method: "address_verification".into(),
                document_revision: None,
                portrait_revision: None,
            },
        )]),
        current_document_revision: None,
        current_portrait_revision: None,
        screening: None,
    };
    let catalog = crate::providers::Catalog {
        endpoints: BTreeMap::from([(
            CheckKind::Address,
            BTreeMap::from([
                ("address".to_owned(), "http://old".to_owned()),
                ("postal-v2".to_owned(), "http://new".to_owned()),
            ]),
        )]),
        demo_callbacks: false,
    };
    assert!(validated(snap.clone(), &catalog).is_ok());
    let asset = snap.evidence.get_mut(&CheckKind::Address).unwrap();
    asset.provider = "postal-v2".into();
    asset.provider_ref = "demo:interactive".into();
    asset.callback = true;
    let bound = validated(snap.clone(), &catalog).unwrap().0;
    assert_eq!(bound[&CheckKind::Address].provider, "postal-v2");
    assert!(!bound[&CheckKind::Address].callback);
    snap.evidence.get_mut(&CheckKind::Address).unwrap().provider = "unconfigured".into();
    assert_eq!(validated(snap, &catalog), Err(EvidenceError::Invalid));
}

#[test]
fn external_policy_directory_supports_new_products_and_rejects_ambiguous_active_versions() {
    let path = std::env::temp_dir().join(format!(
        "ekyc-worker-policies-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap()
    ));
    fs::create_dir(&path).unwrap();
    assert!(Policies::from_directory(&path).is_err());
    let mut policy = Policy::load("wallet", "onboarding").unwrap();
    policy.product = "savings".into();
    policy.version = "savings-1".into();
    fs::write(
        path.join("first.json"),
        serde_json::to_vec(&policy).unwrap(),
    )
    .unwrap();
    let registry = Policies::from_directory(&path).unwrap();
    assert_eq!(
        registry.select("savings", "onboarding").unwrap().version,
        "savings-1"
    );
    assert!(registry.select("wallet", "onboarding").is_err());
    policy.version = "savings-2".into();
    fs::write(
        path.join("duplicate.json"),
        serde_json::to_vec(&policy).unwrap(),
    )
    .unwrap();
    assert!(Policies::from_directory(&path).is_err());
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn reviewer_must_identify_the_current_screening_operation() {
    assert!(review_operation(Some("case/sanctions/2"), "case/sanctions/1").is_err());
    assert!(review_operation(None, "case/sanctions/2").is_err());
    assert!(review_operation(Some("case/sanctions/2"), "case/sanctions/2").is_ok());
}

#[test]
fn mutation_fingerprints_distinguish_amendments_and_reviewers() {
    let command = json!({"institution":"demo","product":"wallet","caller":"operations","key":"retry-1",
            "action":{"type":"review","check":"sanctions","operation_id":"case/sanctions/1","disposition":"clear","reason":"namesake","review_ref":"ticket-4"}});
    let first: Command = serde_json::from_value(command.clone()).unwrap();
    let mut amended = command.clone();
    amended["action"]["review_ref"] = json!("ticket-5");
    let changed: Command = serde_json::from_value(amended).unwrap();
    assert_ne!(first, changed);
    let mut second_caller = command;
    second_caller["caller"] = json!("second-reviewer");
    let second: Command = serde_json::from_value(second_caller).unwrap();
    assert_ne!(first.receipt_key(), second.receipt_key());
}

#[test]
fn interactive_deadline_is_per_attempt_and_other_provider_budgets_stay_short() {
    let now = Utc::now();
    let make_asset = |kind: CheckKind, method: &str, vendor_ref: &str| BoundEvidence {
        tenant_id: "bank".into(),
        subject_ref: "customer".into(),
        kind,
        provider: kind.as_str().into(),
        evidence_ref: format!("{}-issuer-token", kind.as_str()),
        revision: "v1".into(),
        callback: vendor_ref == "demo:interactive",
        provider_ref: vendor_ref.into(),
        method: method.into(),
        document_revision: Some("document-v1".into()),
        portrait_revision: Some("portrait-v1".into()),
    };
    let inputs = BTreeMap::from([
        (
            CheckKind::Document,
            make_asset(
                CheckKind::Document,
                "document_verification",
                "demo:interactive",
            ),
        ),
        (
            CheckKind::Face,
            make_asset(CheckKind::Face, "face_comparison", "vendor-ref"),
        ),
    ]);
    let case = Case::open(
        "case".into(),
        "bank".into(),
        "customer".into(),
        Policy::load("wallet", "onboarding").unwrap(),
        inputs,
        &[],
        BindingSnapshot {
            current_document_revision: Some("document-v1".into()),
            current_portrait_revision: Some("portrait-v1".into()),
            screening: None,
        },
        now,
    )
    .unwrap();
    let mut stored = Stored {
        case,
        opener: "wallet".into(),
        retry: RetrySettings {
            case_deadline_ms: 1000,
            max_attempts: 3,
            retry_base_ms: 50,
        },
        flights: BTreeMap::new(),
    };
    let jobs = stored.schedule(now);
    let job = |kind| jobs.iter().find(|j| j.kind == kind).unwrap();
    assert_eq!(
        job(CheckKind::Document).deadline_ms - now.timestamp_millis(),
        3_600_000
    );
    assert_eq!(
        job(CheckKind::Face).deadline_ms - now.timestamp_millis(),
        1000
    );
    assert_eq!(job(CheckKind::Document).operation_id, "case/document/1");
    assert!(stored.schedule(now).is_empty());
}

#[test]
fn a_match_already_waiting_for_review_is_kept_as_an_alert_when_identity_is_superseded() {
    let now = Utc::now();
    let doc = BoundEvidence {
        tenant_id: "bank".into(),
        subject_ref: "customer".into(),
        kind: CheckKind::Document,
        evidence_ref: "doc".into(),
        revision: "doc1".into(),
        provider: "document".into(),
        callback: false,
        provider_ref: "document-asset".into(),
        method: "document_verification".into(),
        document_revision: Some("doc1".into()),
        portrait_revision: Some("face1".into()),
    };
    let screen = BoundEvidence {
        kind: CheckKind::Sanctions,
        provider: "sanctions".into(),
        method: "sanctions_screening".into(),
        ..doc.clone()
    };
    let bindings = BindingSnapshot {
        current_document_revision: Some("doc1".into()),
        current_portrait_revision: Some("face1".into()),
        screening: Some(screen),
    };
    let mut original = Case::open(
        "case".into(),
        "bank".into(),
        "customer".into(),
        Policy::load("wallet", "onboarding").unwrap(),
        BTreeMap::from([(CheckKind::Document, doc)]),
        &[],
        bindings,
        now,
    )
    .unwrap();
    original
        .record(
            CheckKind::Document,
            "case/document/1",
            Observation::Document {
                passed: true,
                confidence: 99.0,
                expires_on: now.date_naive(),
            },
            now,
            now,
        )
        .unwrap();
    original
        .record(
            CheckKind::Sanctions,
            "case/sanctions/1",
            Observation::Sanctions { hit: true },
            now,
            now,
        )
        .unwrap();
    let mut changed = original.clone();
    changed
        .refresh_bindings(BindingSnapshot::default(), now)
        .unwrap();
    assert_eq!(
        dispatch::superseded_screen(&original, &changed)
            .map(|fact| (fact.operation_id.as_str(), fact.observed_at)),
        Some(("case/sanctions/1", now))
    );
    assert_eq!(dispatch::superseded_screen(&original, &original), None);
    let mut resolved = original.clone();
    resolved
        .review(
            CheckKind::Sanctions,
            ekyc_domain::product::ReviewAction::Clear,
            "staff",
            "different identity",
            "ticket",
            now,
        )
        .unwrap();
    assert_eq!(dispatch::superseded_screen(&original, &resolved), None);
}

#[test]
fn issuer_method_drift_settles_positive_and_deadline_results_as_retryable() {
    use ekyc_domain::product::CheckStatus;
    let at = Utc::now();
    let doc = BoundEvidence {
        tenant_id: "bank".into(),
        subject_ref: "customer".into(),
        kind: CheckKind::Document,
        evidence_ref: "doc".into(),
        revision: "doc1".into(),
        provider: "document".into(),
        callback: false,
        provider_ref: "doc-asset".into(),
        method: "document_verification".into(),
        document_revision: Some("doc1".into()),
        portrait_revision: None,
    };
    let screening = BoundEvidence {
        kind: CheckKind::Sanctions,
        provider: "sanctions".into(),
        evidence_ref: "screen".into(),
        method: "sanctions_screening".into(),
        ..doc.clone()
    };
    let bindings = BindingSnapshot {
        current_document_revision: Some("doc1".into()),
        current_portrait_revision: None,
        screening: Some(screening),
    };
    let mut pending = Case::open(
        "case".into(),
        "bank".into(),
        "customer".into(),
        Policy::load("wallet", "onboarding").unwrap(),
        BTreeMap::from([(CheckKind::Document, doc)]),
        &[],
        bindings.clone(),
        at,
    )
    .unwrap();
    pending
        .record(
            CheckKind::Document,
            "case/document/1",
            Observation::Document {
                passed: true,
                confidence: 99.0,
                expires_on: at.date_naive() + chrono::Duration::days(30),
            },
            at,
            at,
        )
        .unwrap();
    let mut changed = bindings.clone();
    changed.screening.as_mut().unwrap().method = "renamed_screen".into();
    for (observation, positive) in [
        (Observation::Sanctions { hit: true }, true),
        (Observation::Unavailable, false),
    ] {
        let mut case = pending.clone();
        let result = ProductResult {
            operation: Operation {
                kind: CheckKind::Sanctions,
                id: "case/sanctions/1".into(),
            },
            observation,
            observed_at: at,
            received_at: Some(at),
        };
        let error = settlement::settlement_bindings(&case, changed.clone()).unwrap_err();
        settlement::record_binding_failure(&mut case, &result, at, error).unwrap();
        let check = &case.checks[&CheckKind::Sanctions];
        assert_eq!(check.status, CheckStatus::Blocked);
        assert_eq!(check.reasons, ["INVALID_EVIDENCE_BINDING"]);
        assert_eq!(result.is_positive_screen(), positive);
        case.refresh_bindings(bindings.clone(), at).unwrap();
        case.retry(CheckKind::Sanctions, at).unwrap();
        assert_eq!(
            case.checks[&CheckKind::Sanctions].operation_id.as_deref(),
            Some("case/sanctions/2")
        );
    }
}

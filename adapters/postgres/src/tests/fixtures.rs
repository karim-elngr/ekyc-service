use crate::CaseScope;
use chrono::{DateTime, Duration, Utc};
use ekyc_domain::product::{BindingSnapshot, BoundEvidence, CaseState, Fact, Policy, ProductCase};
use ekyc_domain::{CheckKind, Observation};
use std::collections::BTreeMap;

pub(super) fn product_scope(tenant: &str, product: &str) -> CaseScope {
    CaseScope {
        institution: tenant.into(),
        products: vec![product.into()],
        owner: None,
    }
}
pub(super) fn now() -> DateTime<Utc> {
    "2026-11-15T12:00:00Z".parse().unwrap()
}
fn asset(kind: CheckKind) -> BoundEvidence {
    let method = match kind {
        CheckKind::Document => "document_verification",
        CheckKind::Face => "face_comparison",
        CheckKind::Address => "address_verification",
        CheckKind::Sanctions => "sanctions_screening",
    };
    BoundEvidence {
        tenant_id: "institution-a".into(),
        subject_ref: "customer-a".into(),
        kind,
        evidence_ref: format!("asset-{}", kind.as_str()),
        revision: "version-1".into(),
        provider: format!("{}-vendor", kind.as_str()),
        callback: false,
        provider_ref: "internal-vendor-locator".into(),
        method: method.into(),
        document_revision: (kind != CheckKind::Address).then(|| "id-3".into()),
        portrait_revision: matches!(kind, CheckKind::Face | CheckKind::Document)
            .then(|| "portrait-3".into()),
    }
}
pub(super) fn open(
    id: &str,
    product: &str,
    evidence: &[CheckKind],
    facts: &[Fact],
    at: DateTime<Utc>,
) -> ProductCase {
    let use_case = if product == "wallet" {
        "onboarding"
    } else {
        "loan_application"
    };
    let snapshot = BindingSnapshot {
        current_document_revision: Some("id-3".into()),
        current_portrait_revision: Some("portrait-3".into()),
        screening: Some(asset(CheckKind::Sanctions)),
    };
    ProductCase::open(
        id.into(),
        "institution-a".into(),
        "customer-a".into(),
        Policy::load(product, use_case).unwrap(),
        evidence
            .iter()
            .map(|kind| (*kind, asset(*kind)))
            .collect::<BTreeMap<_, _>>(),
        facts,
        snapshot,
        at,
    )
    .unwrap()
}
pub(super) fn settle(
    case: &mut ProductCase,
    kind: CheckKind,
    observation: Observation,
    at: DateTime<Utc>,
) {
    let op = case.checks[&kind].operation_id.clone().unwrap();
    case.record(kind, &op, observation, at, at).unwrap();
}
pub(super) fn passing(kind: CheckKind) -> Observation {
    match kind {
        CheckKind::Document => Observation::Document {
            passed: true,
            confidence: 94.0,
            expires_on: now().date_naive() + Duration::days(365),
        },
        CheckKind::Face => Observation::Face {
            passed: true,
            confidence: 92.0,
            similarity: 99.0,
        },
        CheckKind::Address => Observation::Address {
            passed: true,
            confidence: 95.0,
            proof_date: now().date_naive(),
        },
        CheckKind::Sanctions => Observation::Sanctions { hit: false },
    }
}
pub(super) fn approve_wallet(id: &str) -> ProductCase {
    use CheckKind::*;
    let mut wallet = open(id, "wallet", &[Document, Face], &[], now());
    for kind in [Document, Face, Sanctions] {
        settle(&mut wallet, kind, passing(kind), now());
    }
    assert_eq!(wallet.state, CaseState::Approved);
    wallet
}

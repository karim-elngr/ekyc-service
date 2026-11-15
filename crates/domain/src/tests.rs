use super::*;

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 15).unwrap()
}

fn face(confidence: f64, similarity: f64) -> Observation {
    Observation::Face {
        passed: true,
        confidence,
        similarity,
    }
}

fn address(confidence: f64, age_days: i64) -> Observation {
    Observation::Address {
        passed: true,
        confidence,
        proof_date: today() - chrono::Duration::days(age_days),
    }
}

fn assess_one(check: CheckKind, observation: Observation) -> Decision {
    evaluate(&BTreeMap::from([(check, observation)]), &[check], today())
        .expect("a single settled check must have a decision")
}

fn full_facts() -> BTreeMap<CheckKind, Observation> {
    BTreeMap::from([
        (
            CheckKind::Document,
            Observation::Document {
                passed: true,
                confidence: 85.1,
                expires_on: today(),
            },
        ),
        (CheckKind::Face, face(85.1, 85.1)),
        (CheckKind::Address, address(80.1, 90)),
        (CheckKind::Sanctions, Observation::Sanctions { hit: false }),
    ])
}

#[test]
fn full_scope_approves_on_inclusive_date_boundaries() {
    let decision = evaluate(&full_facts(), Purpose::FullOnboarding.checks(), today()).unwrap();
    assert_eq!(decision.verdict, Verdict::Approved);
    assert!(decision.reasons.is_empty());
}

#[test]
fn incomplete_or_empty_scope_cannot_approve() {
    let mut facts = full_facts();
    facts.remove(&CheckKind::Sanctions);
    assert_eq!(
        evaluate(&facts, Purpose::FullOnboarding.checks(), today()),
        None
    );
    assert_eq!(evaluate(&facts, &[], today()), None);
}

#[test]
fn review_waits_for_remaining_checks() {
    let facts = BTreeMap::from([(CheckKind::Face, face(87.0, 84.0))]);
    assert_eq!(
        evaluate(&facts, Purpose::FullOnboarding.checks(), today()),
        None
    );
}

#[test]
fn a_hit_and_an_unavailable_screen_have_different_rejection_reasons() {
    for (observation, code) in [
        (Observation::Sanctions { hit: true }, "SANCTIONS_HIT"),
        (Observation::Unavailable, "SANCTIONS_UNAVAILABLE"),
    ] {
        let facts = BTreeMap::from([(CheckKind::Sanctions, observation)]);
        let result = evaluate(&facts, Purpose::FullOnboarding.checks(), today()).unwrap();
        assert_eq!(result.verdict, Verdict::Rejected);
        assert_eq!(result.reasons[0].code, code);
    }
}

#[test]
fn expired_document_dominates_later_review_reasons() {
    let mut facts = full_facts();
    facts.insert(
        CheckKind::Document,
        Observation::Document {
            passed: true,
            confidence: 99.0,
            expires_on: today().pred_opt().unwrap(),
        },
    );
    facts.insert(CheckKind::Face, face(50.0, 99.0));
    let result = evaluate(&facts, Purpose::FullOnboarding.checks(), today()).unwrap();
    assert_eq!(result.verdict, Verdict::Rejected);
    assert_eq!(result.reasons[0].code, "DOCUMENT_EXPIRED");
    assert_eq!(result.reasons[1].code, "FACE_CONFIDENCE_LOW");
}

#[test]
fn thresholds_and_proof_dates_are_exact() {
    for (kind, observation, code) in [
        (
            CheckKind::Document,
            Observation::Document {
                passed: true,
                confidence: 85.0,
                expires_on: today(),
            },
            "DOCUMENT_CONFIDENCE_LOW",
        ),
        (CheckKind::Face, face(85.0, 86.0), "FACE_CONFIDENCE_LOW"),
        (CheckKind::Face, face(86.0, 85.0), "FACE_SIMILARITY_LOW"),
        (
            CheckKind::Address,
            address(80.0, 0),
            "ADDRESS_CONFIDENCE_LOW",
        ),
        (CheckKind::Address, address(95.0, 91), "ADDRESS_PROOF_STALE"),
        (
            CheckKind::Address,
            address(95.0, -1),
            "ADDRESS_PROOF_FUTURE",
        ),
    ] {
        let result = assess_one(kind, observation);
        assert_eq!(result.verdict, Verdict::ManualReview);
        assert_eq!(result.reasons[0].code, code);
    }
}

#[test]
fn provider_status_and_confidence_are_independent() {
    let observation = Observation::Face {
        passed: false,
        confidence: 100.0,
        similarity: 100.0,
    };
    let result = assess_one(CheckKind::Face, observation);
    assert_eq!(result.verdict, Verdict::ManualReview);
    assert_eq!(result.reasons[0].code, "FACE_NOT_VERIFIED");
}

#[test]
fn bad_scores_and_mismatched_observations_cannot_approve() {
    for value in [-0.1, 100.1, f64::NAN, f64::INFINITY] {
        let result = assess_one(CheckKind::Face, face(value, 96.0));
        assert_eq!(result.verdict, Verdict::ManualReview);
        assert_eq!(result.reasons[0].code, "INVALID_SCORE");
    }
    assert_eq!(
        assess_one(CheckKind::Sanctions, face(95.0, 96.0)).verdict,
        Verdict::Rejected
    );
}

#[test]
fn missing_evidence_and_provider_outage_preserve_provenance() {
    for (check, fact, verdict, code) in [
        (
            CheckKind::Sanctions,
            Observation::Missing,
            Verdict::Rejected,
            "MISSING_EVIDENCE",
        ),
        (
            CheckKind::Document,
            Observation::Missing,
            Verdict::ManualReview,
            "MISSING_EVIDENCE",
        ),
        (
            CheckKind::Face,
            Observation::Unavailable,
            Verdict::ManualReview,
            "CHECK_UNAVAILABLE",
        ),
    ] {
        let result = assess_one(check, fact);
        assert_eq!(result.verdict, verdict);
        assert_eq!(result.reasons[0].code, code);
    }
}

#[test]
fn narrower_scope_ignores_unrelated_observations() {
    let mut facts = full_facts();
    facts.insert(CheckKind::Sanctions, Observation::Sanctions { hit: true });
    assert_eq!(
        evaluate(&facts, Purpose::AddressRefresh.checks(), today())
            .unwrap()
            .verdict,
        Verdict::Approved
    );
}

#[test]
fn accepted_input_can_omit_evidence_but_cannot_expand_scope() {
    let mut case: OpenCase =
        serde_json::from_str(r#"{"subject_ref":"subject_test","purpose":"address_refresh"}"#)
            .unwrap();
    assert_eq!(case.validate(), Ok(()));
    case.evidence.insert(CheckKind::Face, "ev_1".into());
    assert!(case.validate().is_err());
}

#[test]
fn serialization_has_stable_wire_names_and_checked_fields() {
    let case: OpenCase = serde_json::from_str(
        r#"{"subject_ref":"subject_test","purpose":"full_onboarding","evidence":{"face":"ev_2"}}"#,
    )
    .unwrap();
    assert_eq!(case.evidence[&CheckKind::Face], "ev_2");
    assert_eq!(
        serde_json::to_value(Verdict::ManualReview).unwrap(),
        "MANUAL_REVIEW"
    );
    assert!(serde_json::from_str::<OpenCase>(
        r#"{"subject_ref":"test","purpose":"address_refresh","checks":[]}"#
    )
    .is_err());
}

#[test]
fn projected_progress_and_final_summaries_preserve_scope() {
    let plan = Purpose::FullOnboarding.checks();
    let mut facts = full_facts();
    facts.remove(&CheckKind::Document);
    facts.insert(CheckKind::Face, face(85.0, 92.0));
    facts.insert(CheckKind::Sanctions, Observation::Sanctions { hit: true });
    let results = |date| {
        summarize_checks(&facts, plan, date)
            .into_iter()
            .map(|summary| summary.result)
            .collect::<Vec<_>>()
    };
    use CheckResult::*;
    assert_eq!(results(None), vec![Pending, Observed, Observed, Observed]);
    assert_eq!(
        results(Some(today())),
        vec![SkippedAfterDecision, Review, Pass, Fail]
    );
}

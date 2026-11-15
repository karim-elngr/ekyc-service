use super::fixtures::*;
use crate::Store;
use chrono::Duration;
use ekyc_domain::{
    product::{CaseState, CheckStatus, Origin, ReviewAction},
    CheckKind, Observation,
};
use serde_json::Value;
use sqlx::{types::Json, Error, PgPool};
#[sqlx::test(migrations = "./migrations")]
async fn repeated_and_stale_product_writes_preserve_one_event_per_revision_and_late_alert(
    pool: PgPool,
) -> Result<(), Error> {
    use CheckKind::*;
    let store = Store { pool };
    let initial = open("wallet-origin", "wallet", &[Document, Face], &[], now());
    let terminal = approve_wallet("wallet-origin");
    store.publish_product(&initial).await?;
    let (a, b) = tokio::join!(
        store.publish_product(&terminal),
        store.publish_product(&terminal)
    );
    a?;
    b?;
    store.publish_product(&initial).await?;
    let mut newer = initial.clone();
    newer.revision = terminal.revision + 1;
    store.publish_product(&newer).await?;
    let late_operation = "wallet-origin/sanctions/previous";
    let (a, b) = tokio::join!(
        store.late_product_screen(&terminal, late_operation, now(), Some(now())),
        store.late_product_screen(&terminal, late_operation, now(), Some(now()))
    );
    a?;
    b?;
    let view = store
        .scoped_case(&terminal.case_id, &product_scope("institution-a", "wallet"))
        .await?
        .unwrap();
    assert_eq!(view, terminal.view());
    let revision_count: i64 = sqlx::query_scalar("SELECT count(*) FROM product_case_revision")
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(revision_count, 2);
    let events: Vec<(String, i64, Json<Value>)> = sqlx::query_as(
        "SELECT e.event_id,r.version,e.payload FROM decision_outbox e JOIN product_case_revision r
         ON (e.product_case_id,e.version)=(r.case_id,r.version)
         WHERE e.product_case_id=$1 ORDER BY e.event_id",
    )
    .bind(&terminal.case_id)
    .fetch_all(&store.pool)
    .await?;
    assert_eq!(events.len(), 3);
    for (id, version, Json(event)) in events {
        assert_eq!(event["event_id"], id);
        match event["event_type"].as_str().unwrap() {
            "ekyc.case.action.v2" => {
                assert_eq!(version, initial.revision);
                assert_eq!(event["state"], "RUNNING");
            }
            "ekyc.case.decided.v2" => {
                assert_eq!(version, terminal.revision);
                assert_eq!(event["state"], "APPROVED");
            }
            "ekyc.screening.alert.v2" => {
                assert_eq!(version, terminal.revision);
                assert_eq!(event["operation_id"], late_operation);
            }
            other => panic!("unexpected product event {other}"),
        }
    }
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn only_approved_fresh_identity_checks_are_reusable_and_newer_adverse_checks_supersede_them(
    pool: PgPool,
) -> Result<(), Error> {
    use CheckKind::*;
    let store = Store { pool };
    let mut wallet = open("wallet-origin", "wallet", &[Document, Face], &[], now());
    for kind in [Document, Face] {
        settle(&mut wallet, kind, passing(kind), now());
    }
    store.publish_product(&wallet).await?;
    assert!(store
        .product_facts("institution-a", "customer-a")
        .await?
        .is_empty());
    settle(&mut wallet, Sanctions, passing(Sanctions), now());
    store.publish_product(&wallet).await?;
    let facts = store.product_facts("institution-a", "customer-a").await?;
    assert!(store
        .product_facts("institution-b", "customer-a")
        .await?
        .is_empty());
    assert!(store
        .product_facts("institution-a", "other-subject")
        .await?
        .is_empty());
    let mut loan = open("loan-approved", "lending", &[Address], &facts, now());
    assert_eq!(loan.checks[&Face].origin, Some(Origin::Reused));
    let mut pending = open("loan-pending", "lending", &[], &facts, now());
    for kind in [Address, Sanctions] {
        settle(&mut loan, kind, passing(kind), now());
    }
    store.publish_product(&loan).await?;
    let all = store.product_facts("institution-a", "customer-a").await?;
    let face_rows: Vec<_> = all.iter().filter(|f| f.evidence.kind == Face).collect();
    assert_eq!(face_rows.len(), 1);
    let source_face = wallet.checks[&Face].fact.as_ref().unwrap();
    assert_eq!(face_rows[0], source_face);
    assert_eq!(loan.checks[&Face].fact.as_ref().unwrap(), source_face);
    let later = now() + Duration::days(1);
    let mut changed = open("second-wallet", "wallet", &[Document, Face], &[], later);
    settle(
        &mut changed,
        Face,
        Observation::Face {
            passed: false,
            confidence: 99.0,
            similarity: 99.0,
        },
        later,
    );
    assert_eq!(changed.state, CaseState::AwaitingEvidence);
    store.publish_product(&changed).await?;
    store.publish_product(&changed).await?;
    let updated = store.product_facts("institution-a", "customer-a").await?;
    assert_eq!(
        updated
            .iter()
            .filter(|f| f.origin_case_id == "second-wallet" && !f.source_passed)
            .count(),
        1
    );
    pending.revalidate_reuse(&updated, later);
    assert_eq!(pending.checks[&Face].status, CheckStatus::AwaitingEvidence);
    assert_eq!(pending.checks[&Document].origin, Some(Origin::Reused));
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn scoped_reviewer_history_preserves_evidence_and_attribution_while_public_view_omits_them(
    pool: PgPool,
) -> Result<(), Error> {
    use CheckKind::*;
    let store = Store { pool };
    let mut c = open("review-wallet", "wallet", &[Document, Face], &[], now());
    for kind in [Document, Face] {
        settle(&mut c, kind, passing(kind), now());
    }
    settle(
        &mut c,
        Sanctions,
        Observation::Sanctions { hit: true },
        now(),
    );
    store.publish_product(&c).await?;
    c.review(
        Sanctions,
        ReviewAction::Clear,
        "reviewer-a",
        "Different DOB",
        "investigation-9",
        now(),
    )
    .unwrap();
    store.publish_product(&c).await?;
    for (tenant, product) in [("institution-b", "wallet"), ("institution-a", "lending")] {
        assert!(store
            .product_review_details(&c.case_id, tenant, product)
            .await?
            .is_none());
        assert!(store
            .scoped_case(&c.case_id, &product_scope(tenant, product))
            .await?
            .is_none());
    }
    let public = store
        .scoped_case(&c.case_id, &product_scope("institution-a", "wallet"))
        .await?
        .unwrap();
    assert!(public.get("reviews").is_none());
    for check in public["checks"].as_array().unwrap() {
        assert!(check.get("evidence_ref").is_none());
        assert!(check.get("provider_ref").is_none());
    }
    let screen_op = c.checks[&CheckKind::Sanctions]
        .operation_id
        .as_deref()
        .unwrap();
    store
        .late_product_screen(&c, screen_op, now(), Some(now()))
        .await?;
    let review = store
        .product_review_details(&c.case_id, "institution-a", "wallet")
        .await?
        .unwrap();
    assert_eq!(review["version"], c.revision);
    assert_eq!(review["state"], "APPROVED");
    let alert = &review["alerts"][0];
    assert_eq!(alert["operation_id"], screen_op);
    assert_eq!(alert["evidence_ref"], "asset-sanctions");
    assert!(alert.get("provider_ref").is_none());
    let history = &review["reviews"][0];
    assert_eq!(history["actor"], "reviewer-a");
    assert_eq!(history["review_ref"], "investigation-9");
    assert_eq!(history["reason"], "Different DOB");
    assert_eq!(history["action"], "clear");
    let screen = review["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "sanctions")
        .unwrap();
    assert_eq!(screen["evidence_ref"], "asset-sanctions");
    assert_eq!(screen["operation_id"], history["operation_id"]);
    assert_eq!(screen["evidence_revision"], "version-1");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_adverse_commit_prevents_stale_reuse_approval(
    pool: PgPool,
) -> Result<(), Error> {
    use CheckKind::*;
    let store = Store { pool: pool.clone() };
    let at = chrono::Utc::now() - Duration::minutes(1);
    let mut wallet = open("origin", "wallet", &[Document, Face], &[], at);
    settle(
        &mut wallet,
        Document,
        Observation::Document {
            passed: true,
            confidence: 99.0,
            expires_on: at.date_naive() + Duration::days(30),
        },
        at,
    );
    settle(&mut wallet, Face, passing(Face), at);
    settle(&mut wallet, Sanctions, passing(Sanctions), at);
    store.publish_product(&wallet).await?;

    let mut loan = open(
        "contended-loan",
        "lending",
        &[Address],
        &wallet.fresh_passes(),
        at,
    );
    let address = Observation::Address {
        passed: true,
        confidence: 99.0,
        proof_date: at.date_naive(),
    };
    settle(&mut loan, Address, address, at);
    settle(&mut loan, Sanctions, passing(Sanctions), at);
    assert_eq!(loan.state, CaseState::Approved);

    // A simultaneous writer holds the subject generation lock while the loan
    // attempts to publish the older snapshot. This writer eventually commits.
    let mut writer = pool.begin().await?;
    let mut adverse = wallet.checks[&Face].fact.clone().unwrap();
    adverse.operation_id = "origin/face/2".into();
    adverse.observed_at = at + Duration::seconds(1);
    adverse.source_passed = false;
    sqlx::query(
        "INSERT INTO product_fact(operation_id,origin_case_id,institution,subject_ref,fact)
                 VALUES($1,'origin','institution-a','customer-a',$2)",
    )
    .bind(&adverse.operation_id)
    .bind(Json(&adverse))
    .execute(&mut *writer)
    .await?;
    let publishing = store.commit_assessment(&loan);
    tokio::pin!(publishing);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(80), &mut publishing)
            .await
            .is_err()
    );
    writer.commit().await?;
    let effective = publishing.await?;
    assert_eq!(effective.state, CaseState::AwaitingEvidence);
    assert_eq!(effective.checks[&Face].reasons, ["REUSE_SUPERSEDED"]);
    assert_eq!(store.commit_assessment(&loan).await?, effective);
    let (decisions, actions): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE payload->>'event_type'='ekyc.case.decided.v2'),
                count(*) FILTER (WHERE payload->>'event_type'='ekyc.case.action.v2')
         FROM decision_outbox WHERE product_case_id='contended-loan'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!((decisions, actions), (0, 1));
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn bounded_candidates_match_full_history_across_pages_and_old_facts(
    pool: PgPool,
) -> Result<(), Error> {
    use crate::ReuseQuery;
    use CheckKind::*;
    let store = Store { pool: pool.clone() };
    let wallet = approve_wallet("source");
    store.publish_product(&wallet).await?;
    let face = wallet.checks[&Face].fact.as_ref().unwrap();
    // Three years of irrelevant facts, and more than one SQL page of tied
    // current passes. The adverse witness sorts behind all pages of tied passes.
    sqlx::query(
        "INSERT INTO product_fact(operation_id,origin_case_id,institution,subject_ref,fact)
        SELECT 'old/'||i,'source','institution-a','customer-a',$1::jsonb || jsonb_build_object(
            'operation_id','old/'||i,'observed_at','2023-11-01T00:00:00Z')
        FROM generate_series(1,3500) i",
    )
    .bind(Json(face))
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO product_fact(operation_id,origin_case_id,institution,subject_ref,fact)
        SELECT 'zz/'||i,'source','institution-a','customer-a',$1::jsonb || jsonb_build_object('operation_id','zz/'||i)
        FROM generate_series(1,150) i")
        .bind(Json(face)).execute(&pool).await?;
    let mut adverse = face.clone();
    adverse.operation_id = "a".into();
    adverse.source_passed = false;
    sqlx::query(
        "INSERT INTO product_fact VALUES($1,'source','institution-a','customer-a',$2,now())",
    )
    .bind(&adverse.operation_id)
    .bind(Json(&adverse))
    .execute(&pool)
    .await?;
    let start = std::time::Instant::now();
    let all = store.product_facts("institution-a", "customer-a").await?;
    let historical_ms = start.elapsed().as_millis();
    let original = open("new-loan", "lending", &[], &wallet.fresh_passes(), now());
    let mut query = ReuseQuery::pinned(&original, now());
    query.pinned = None;
    let start = std::time::Instant::now();
    let shortlist = store.reuse_candidates(&query).await?;
    let bounded_ms = start.elapsed().as_millis();
    assert_eq!(shortlist.len(), 2);
    assert_eq!(
        open("new-loan", "lending", &[], &shortlist, now()),
        open("new-loan", "lending", &[], &all, now())
    );
    let mut pinned = original.clone();
    let witnesses = store
        .reuse_candidates(&ReuseQuery::pinned(&pinned, now()))
        .await?;
    assert!(witnesses.len() <= 4);
    pinned.revalidate_reuse(&witnesses, now());
    assert_eq!(pinned.checks[&Face].status, CheckStatus::AwaitingEvidence);
    assert_eq!(pinned.checks[&Document].origin, Some(Origin::Reused));
    eprintln!("long-history fixture: complete {} rows/{} bytes/{} ms; candidate result {} rows/{} bytes/{} ms",
        all.len(), serde_json::to_vec(&all).unwrap().len(), historical_ms, shortlist.len(),
        serde_json::to_vec(&shortlist).unwrap().len(), bounded_ms);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn reviewer_provenance_survives_switching_vendors_for_same_kind(
    pool: PgPool,
) -> Result<(), Error> {
    use CheckKind::*;
    let store = Store { pool };
    for (id, vendor) in [("old-source", "identity-a"), ("new-source", "identity-b")] {
        let mut wallet = open(id, "wallet", &[Document, Face], &[], now());
        wallet
            .checks
            .get_mut(&Document)
            .unwrap()
            .evidence
            .as_mut()
            .unwrap()
            .provider = vendor.into();
        for kind in [Document, Face, Sanctions] {
            settle(&mut wallet, kind, passing(kind), now());
        }
        store.publish_product(&wallet).await?;
    }
    for (id, vendor) in [("old-source", "identity-a"), ("new-source", "identity-b")] {
        let handoff = store
            .product_review_details(id, "institution-a", "wallet")
            .await?
            .unwrap();
        let check = handoff["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["kind"] == "document")
            .unwrap();
        assert_eq!(check["provider"], vendor);
        assert_eq!(check["method"], "document_verification");
        assert!(check["received_at"].is_string());
    }
    Ok(())
}

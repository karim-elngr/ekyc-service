use super::fixtures::*;
use crate::Store;
use chrono::Duration;
use ekyc_domain::CheckKind;
use sqlx::{Error, PgPool};
#[sqlx::test(migrations = "./migrations")]
async fn directory_and_overview_share_scope_time_filters_and_stable_order(
    pool: PgPool,
) -> Result<(), Error> {
    use crate::{CaseFilter, CaseScope, PageOrder};
    let store = Store { pool };
    let wallet = format!("v2~wallet~{:064x}", 1);
    let sibling = format!("v2~sibling~{:064x}", 2);
    let loan = format!("v2~loan~{:064x}", 3);
    for (id, product, day, tenant) in [
        (wallet.as_str(), "wallet", 0, "institution-a"),
        (sibling.as_str(), "wallet", 0, "institution-a"),
        (loan.as_str(), "lending", 0, "institution-a"),
        ("outside-window", "wallet", 1, "institution-a"),
        ("foreign", "wallet", 0, "other-bank"),
    ] {
        let mut c = open(id, product, &[], &[], now() + Duration::days(day));
        c.tenant_id = tenant.to_owned();
        store.publish_product(&c).await?;
    }
    let scope = CaseScope {
        institution: "institution-a".into(),
        products: vec!["wallet".into(), "lending".into()],
        owner: None,
    };
    let mut filter = CaseFilter {
        from: now(),
        to: now() + Duration::days(1),
        state: None,
        use_case: None,
        query: None,
    };
    let page = store
        .case_page(&scope, &filter, PageOrder::CreatedAtDesc, 2, 0)
        .await?;
    assert_eq!(page.total, 3);
    assert_eq!(
        page.cases
            .iter()
            .map(|v| v["case_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [wallet.as_str(), sibling.as_str()]
    );
    assert_eq!(
        store
            .case_page(&scope, &filter, PageOrder::CreatedAtDesc, 2, 4)
            .await?
            .total,
        3
    );
    let ascending = store
        .case_page(&scope, &filter, PageOrder::CreatedAtAsc, 1, 0)
        .await?;
    assert_eq!(ascending.cases[0]["case_id"], loan);
    let overview = store.case_overview(&scope, &filter).await?;
    assert_eq!(overview.total, 3);
    assert_eq!(overview.states.values().sum::<i64>(), 3);
    assert_eq!(overview.checks.iter().map(|c| c.count).sum::<i64>(), 10);
    let owner = CaseScope {
        owner: Some("wallet".into()),
        ..scope.clone()
    };
    assert_eq!(
        store
            .case_page(&owner, &filter, PageOrder::CreatedAtAsc, 20, 0)
            .await?
            .total,
        1
    );
    filter.state = Some("REJECTED".into());
    assert_eq!(store.case_overview(&scope, &filter).await?.total, 0);
    filter.state = None;
    filter.query = Some("%_".into());
    assert_eq!(store.case_overview(&scope, &filter).await?.total, 0);
    filter.query = Some("CUSTOMER-A".into());
    filter.use_case = Some("loan_application".into());
    assert_eq!(store.case_overview(&scope, &filter).await?.total, 1);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn history_window_is_half_open_and_cursor_stays_within_the_grant(
    pool: PgPool,
) -> Result<(), Error> {
    let store = Store { pool };
    let initial = open(
        "history",
        "wallet",
        &[CheckKind::Document, CheckKind::Face],
        &[],
        now(),
    );
    let mut final_case = initial.clone();
    for kind in [CheckKind::Document, CheckKind::Face, CheckKind::Sanctions] {
        settle(&mut final_case, kind, passing(kind), now());
    }
    for (case, time) in [(&initial, now()), (&final_case, now() + Duration::days(1))] {
        store.publish_product(case).await?;
        sqlx::query(
            "UPDATE product_case_revision SET recorded_at=$1 WHERE case_id=$2 AND version=$3",
        )
        .bind(time)
        .bind(&case.case_id)
        .bind(case.revision)
        .execute(&store.pool)
        .await?;
    }
    let scope = product_scope("institution-a", "wallet");
    let window = Some((now(), now() + Duration::days(1)));
    let selected = store
        .case_history("history", &scope, None, window, 10)
        .await?
        .unwrap();
    assert_eq!(selected.revisions.len(), 1);
    assert_eq!(selected.revisions[0].view, initial.view());
    let page = store
        .case_history("history", &scope, None, None, 1)
        .await?
        .unwrap();
    assert_eq!(page.next_before_version, Some(final_case.revision));
    assert_eq!(
        store
            .case_history("history", &scope, page.next_before_version, window, 1)
            .await?
            .unwrap()
            .revisions[0]
            .version,
        initial.revision
    );
    assert!(store
        .case_history(
            "history",
            &product_scope("other", "wallet"),
            None,
            window,
            10
        )
        .await?
        .is_none());
    Ok(())
}

use super::{Error, Json};
use ekyc_domain::product::{ProductCase, ScreenObligation, ScreenOutcome};
use ekyc_domain::{CheckKind, Observation};
use sqlx::{Postgres, Transaction};

/// All live and restore writers serialize changes to a strict case under the same
/// transaction lock. The lock also covers the first revision before a case row exists.
pub(super) async fn prepare(
    tx: &mut Transaction<'_, Postgres>,
    case: &mut ProductCase,
) -> Result<Option<ProductCase>, Error> {
    if !case.strict_screening() {
        return Ok(None);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,12))")
        .bind(&case.case_id)
        .execute(&mut **tx)
        .await?;
    let previous: Option<Json<ProductCase>> = sqlx::query_scalar(
        "SELECT r.assessment FROM product_case c JOIN product_case_revision r USING(case_id,version) WHERE c.case_id=$1 FOR UPDATE OF c"
    ).bind(&case.case_id).fetch_optional(&mut **tx).await?;
    if let Some(Json(previous)) = previous {
        if previous.revision >= case.revision || previous.is_terminal() {
            return Ok(Some(previous));
        }
        if previous.policy != case.policy
            || previous.tenant_id != case.tenant_id
            || previous.subject_ref != case.subject_ref
        {
            return Err(Error::Protocol(
                "strict case identity or pinned policy changed".into(),
            ));
        }
    }

    let persisted: Vec<Json<ScreenObligation>> = sqlx::query_scalar(
        "SELECT receipt FROM strict_screen_obligation WHERE case_id=$1 ORDER BY original_operation FOR UPDATE"
    ).bind(&case.case_id).fetch_all(&mut **tx).await?;
    for Json(old) in persisted {
        let hold = case
            .screen_obligations
            .entry(old.original_operation.clone())
            .or_insert_with(|| old.clone());
        // Independent proposals can lack information. A later publication cannot erase
        // a completed lookup, its reviewer or the original screening context.
        hold.original_evidence = old.original_evidence;
        hold.armed_at = old.armed_at;
        hold.assign_at = old.assign_at;
        if old.outcome.is_some() {
            hold.outcome = old.outcome;
            hold.observed_at = old.observed_at;
            hold.received_at = old.received_at;
        }
        if old.review.is_some() {
            hold.review = old.review;
        }
        if old.assigned_to.is_some() {
            hold.assigned_to = old.assigned_to;
        }
        if old.replacement_operation.is_some() {
            hold.replacement_operation = old.replacement_operation;
        }
    }
    for hold in case.screen_obligations.values() {
        write_obligation(tx, &case.case_id, hold).await?;
    }
    if let Some(fact) = case
        .checks
        .get(&CheckKind::Sanctions)
        .and_then(|c| c.fact.as_ref())
    {
        if matches!(fact.observation, Observation::Sanctions { .. })
            && case
                .screen_obligations
                .values()
                .any(|h| h.replacement_operation.as_ref() == Some(&fact.operation_id))
        {
            sqlx::query("UPDATE provider_route_selection SET status='complete',result=$2,observed_at=$3,received_at=$4 WHERE operation_id=$1")
                .bind(&fact.operation_id).bind(Json(&fact.observation)).bind(fact.observed_at).bind(fact.received_at).execute(&mut **tx).await?;
        }
    }
    case.guard_screening_decision();
    Ok(None)
}

pub(super) async fn write_obligation(
    tx: &mut Transaction<'_, Postgres>,
    case_id: &str,
    hold: &ScreenObligation,
) -> Result<(), Error> {
    let status = match hold.outcome {
        None => "outstanding",
        Some(ScreenOutcome::Clear) => "clear",
        Some(ScreenOutcome::Hit) => "hit",
        Some(ScreenOutcome::Inconclusive) => "inconclusive",
        Some(ScreenOutcome::NotAccepted) => "not_accepted",
    };
    sqlx::query("INSERT INTO strict_screen_obligation(case_id,original_operation,replacement_operation,status,assigned_to,assign_at,receipt)
            VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(original_operation) DO UPDATE SET
            replacement_operation=EXCLUDED.replacement_operation, status=EXCLUDED.status,
            assigned_to=EXCLUDED.assigned_to, receipt=EXCLUDED.receipt WHERE strict_screen_obligation.case_id=EXCLUDED.case_id")
            .bind(case_id).bind(&hold.original_operation).bind(&hold.replacement_operation)
            .bind(status).bind(&hold.assigned_to).bind(hold.assign_at).bind(Json(hold)).execute(&mut **tx).await?;
    if let Some(outcome) = hold.outcome {
        let (route_status, finding) = match outcome {
            ScreenOutcome::Hit | ScreenOutcome::Clear => (
                "complete",
                Some(Observation::Sanctions {
                    hit: outcome == ScreenOutcome::Hit,
                }),
            ),
            ScreenOutcome::NotAccepted => ("not_accepted", None),
            ScreenOutcome::Inconclusive => ("inconclusive", None),
        };
        sqlx::query("UPDATE provider_route_selection SET status=$2,result=$3,observed_at=$4,received_at=$5 WHERE operation_id=$1")
                .bind(&hold.original_operation).bind(route_status).bind(finding.map(Json)).bind(hold.observed_at).bind(hold.received_at).execute(&mut **tx).await?;
    }
    if let Some(owner) = &hold.assigned_to {
        sqlx::query("UPDATE provider_route_selection SET owner=$2 WHERE operation_id=$1")
            .bind(&hold.original_operation)
            .bind(owner)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

impl super::Store {
    /// Close the selected route under the same case and operation locks used by admission.
    /// A consumed authorization remains ambiguous unless it durably expired before HTTP.
    pub async fn fence_unsent_screen(&self, case: &str, operation: &str) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT 1 FROM product_case WHERE case_id=$1 FOR UPDATE")
            .bind(case)
            .execute(&mut *tx)
            .await?;
        let changed=sqlx::query("UPDATE provider_route_selection s SET retired=true,status='not_accepted',block_reason='FENCED_BEFORE_SEND'
            WHERE case_id=$1 AND operation_id=$2 AND kind='sanctions' AND provider IN ('sanctions-a','sanctions-b') AND
            (status IN ('assigned','retired','blocked','outstanding') OR (status='not_accepted' AND block_reason='FENCED_BEFORE_SEND'))
            AND (NOT possible_send OR (EXISTS(SELECT FROM provider_attempt WHERE operation_id=s.operation_id AND lane='submit' AND class='expired')
            AND NOT EXISTS(SELECT FROM provider_attempt WHERE operation_id=s.operation_id AND lane='submit' AND class<>'expired')))")
            .bind(case).bind(operation).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(changed.rows_affected() == 1)
    }
}

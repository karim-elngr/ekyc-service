use super::{Error, Json, ReuseQuery, Store};
use chrono::{DateTime, Utc};
#[cfg(all(test, feature = "postgres-tests"))]
use ekyc_domain::product::Fact;
use ekyc_domain::product::{CaseState, ProductCase, ScreenObligation, ScreenOutcome};
use serde_json::{json, Value};
use sqlx::{Postgres, Transaction};

type ReviewerRow = (Json<ProductCase>, Json<Value>, Json<Vec<ScreenObligation>>);

impl Store {
    /// Return issuer references and investigation history exclusively through the reviewer grant.
    pub async fn product_review_details(
        &self,
        id: &str,
        institution: &str,
        product: &str,
    ) -> Result<Option<Value>, Error> {
        let row: Option<ReviewerRow> = sqlx::query_as(
            "SELECT r.assessment, COALESCE((
                 SELECT jsonb_agg(jsonb_build_object(
                     'event_id', e.event_id, 'operation_id', e.payload->'operation_id',
                     'observed_at', e.payload->'observed_at','received_at',e.payload->'received_at', 'reason', e.payload->'reason',
                     'provider', original.asset->'provider', 'method', original.asset->'method',
                     'evidence_ref', original.asset->'evidence_ref', 'evidence_revision', original.asset->'revision')
                     ORDER BY e.created_at, e.event_id)
                 FROM decision_outbox e LEFT JOIN LATERAL (
                     SELECT h.assessment #> '{checks,sanctions,evidence}' AS asset
                     FROM product_case_revision h WHERE h.case_id = c.case_id AND
                       h.assessment #>> '{checks,sanctions,operation_id}' = e.payload->>'operation_id'
                     ORDER BY h.version LIMIT 1
                 ) original ON true
                 WHERE e.product_case_id = c.case_id AND e.payload->>'event_type' = 'ekyc.screening.alert.v2'
             ), '[]'::jsonb),
             (SELECT COALESCE(jsonb_agg(h.receipt ORDER BY h.original_operation),'[]'::jsonb) FROM strict_screen_obligation h WHERE h.case_id=c.case_id)
             FROM product_case c JOIN product_case_revision r USING(case_id,version)
             WHERE c.case_id=$1 AND c.institution=$2 AND c.product=$3")
            .bind(id).bind(institution).bind(product).fetch_optional(&self.pool).await?;
        Ok(row.map(|(Json(case), Json(alerts),Json(holds))| {
            let checks: Vec<_> = case.checks.values().filter_map(|check| {
                let fact = check.fact.as_ref()?;
                Some(json!({"kind":check.kind,"status":check.status,"operation_id":fact.operation_id,
                    "origin_case_id":fact.origin_case_id,"provider":fact.provider,"method":fact.evidence.method,
                    "observed_at":fact.observed_at,"received_at":fact.received_at,"evidence_ref":fact.evidence.evidence_ref,
                    "evidence_revision":fact.evidence.revision,"reasons":check.reasons}))
            }).collect();
            let screens: Vec<_> = holds.iter().map(|h| json!({
                "original_operation": h.original_operation, "replacement_operation":h.replacement_operation,
                "provider":h.original_evidence.provider,"method":h.original_evidence.method,
                "evidence_revision":h.original_evidence.revision,"outcome":h.outcome,"assigned_to":h.assigned_to,
                "armed_at":h.armed_at,"assign_at":h.assign_at,"review":h.review
            })).collect();
            json!({"case_id":case.case_id,"subject_ref":case.subject_ref,"version":case.revision,"screening_obligations":screens,
                "state":case.state,"checks":checks,"reviews":case.reviews,"alerts":alerts})
        }))
    }
    #[cfg(all(test, feature = "postgres-tests"))]
    /// Inspect complete persisted facts in projection and migration test fixtures.
    pub async fn product_facts(
        &self,
        institution: &str,
        subject: &str,
    ) -> Result<Vec<Fact>, Error> {
        let rows: Vec<Json<Fact>> = sqlx::query_scalar(
            "SELECT fact FROM product_fact WHERE institution=$1 AND subject_ref=$2 ORDER BY operation_id")
            .bind(institution).bind(subject).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// Publish a live assessment using the same decision-time safeguards as
    /// the workflow worker. Callers needing the effective case use commit_assessment.
    pub async fn publish_product(&self, case: &ProductCase) -> Result<(), Error> {
        self.commit_assessment(case).await.map(|_| ())
    }

    /// Restore an authoritative historical assessment without evaluating it against
    /// evidence observed after the original decision.
    pub async fn restore_product_projection(&self, case: &ProductCase) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        let mut effective = case.clone();
        if super::strict_screen::prepare(&mut tx, &mut effective)
            .await?
            .is_none()
        {
            self.write_product(&mut tx, &effective, None).await?;
        }
        tx.commit().await
    }

    /// Publish and return the exact assessment the worker must store. The
    /// persisted revision acts as the receipt if the SQL commit acknowledgement
    /// was lost before Restate journaled this call.
    pub async fn commit_assessment(&self, proposal: &ProductCase) -> Result<ProductCase, Error> {
        let mut tx = self.pool.begin().await?;
        let mut effective = proposal.clone();
        if let Some(previous) = super::strict_screen::prepare(&mut tx, &mut effective).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let mut generation = None;
        if effective.state == CaseState::Approved && effective.has_reused_checks() {
            sqlx::query("INSERT INTO subject_fact_generation(institution,subject_ref) VALUES($1,$2) ON CONFLICT DO NOTHING")
                .bind(&proposal.tenant_id).bind(&proposal.subject_ref).execute(&mut *tx).await?;
            generation = Some(sqlx::query_scalar::<_,i64>(
                "SELECT generation FROM subject_fact_generation WHERE institution=$1 AND subject_ref=$2 FOR UPDATE")
                .bind(&proposal.tenant_id).bind(&proposal.subject_ref).fetch_one(&mut *tx).await?);
            if let Some(Json(already)) = sqlx::query_scalar::<_, Json<ProductCase>>(
                "SELECT assessment FROM product_case_revision WHERE case_id=$1 AND version=$2",
            )
            .bind(&proposal.case_id)
            .bind(proposal.revision)
            .fetch_optional(&mut *tx)
            .await?
            {
                tx.commit().await?;
                return Ok(already);
            }
            let at: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
                .fetch_one(&mut *tx)
                .await?;
            let facts = ReuseQuery::pinned(proposal, at).select(&mut tx).await?;
            effective.verify_proposed_approval(&facts, at);
        }
        if !self.write_product(&mut tx, &effective, generation).await? {
            let Json(stored) = sqlx::query_scalar::<_,Json<ProductCase>>(
                "SELECT r.assessment FROM product_case c JOIN product_case_revision r USING(case_id,version)
                 WHERE c.case_id=$1")
                .bind(&proposal.case_id).fetch_one(&mut *tx).await?;
            effective = stored;
        }
        tx.commit().await?;
        Ok(effective)
    }

    async fn write_product(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        case: &ProductCase,
        generation: Option<i64>,
    ) -> Result<bool, Error> {
        let view = case.public_view();
        let state = case.state.as_str();
        let revision = case.revision;
        let changed = sqlx::query(
            "INSERT INTO product_case(case_id,institution,product,subject_ref,version,state,public_view,created_at)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(case_id) DO UPDATE SET
             version=excluded.version,state=excluded.state,public_view=excluded.public_view
             WHERE product_case.version < excluded.version AND product_case.state NOT IN ('APPROVED','REJECTED')")
            .bind(&case.case_id).bind(&case.tenant_id).bind(&case.policy.product).bind(&case.subject_ref)
            .bind(revision).bind(state).bind(Json(&view)).bind(case.created_at).execute(&mut **tx).await?;
        if changed.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO product_case_revision(case_id,version,assessment,public_view,fact_generation) VALUES($1,$2,$3,$4,$5)",
        )
        .bind(&case.case_id)
        .bind(revision)
        .bind(Json(case))
        .bind(Json(&view))
        .bind(generation)
        .execute(&mut **tx)
        .await?;

        let mut facts = case.fresh_adverse();
        if state == "APPROVED" {
            facts.extend(case.fresh_passes());
        }
        for fact in facts {
            sqlx::query(
                "INSERT INTO product_fact(operation_id,origin_case_id,institution,subject_ref,fact)
                             VALUES($1,$2,$3,$4,$5) ON CONFLICT(operation_id) DO NOTHING",
            )
            .bind(&fact.operation_id)
            .bind(&case.case_id)
            .bind(&case.tenant_id)
            .bind(&case.subject_ref)
            .bind(Json(&fact))
            .execute(&mut **tx)
            .await?;
        }
        let event_type = if case.is_terminal() {
            "ekyc.case.decided.v2"
        } else {
            "ekyc.case.action.v2"
        };
        let id = format!("{}:{revision}", case.case_id);
        let event = json!({"event_id":id,"event_type":event_type,"case_id":case.case_id,
            "institution":case.tenant_id,"product":case.policy.product,"use_case":case.policy.use_case,
            "version":revision,"policy_version":case.policy.version,"state":state,
            "checks":view.checks,"next_action":view.next_action});
        enqueue(tx, &id, &case.case_id, revision, event).await?;
        for hold in case
            .screen_obligations
            .values()
            .filter(|h| h.outcome == Some(ekyc_domain::product::ScreenOutcome::Hit))
        {
            if let Some(observed) = hold.observed_at {
                enqueue_screen_alert(
                    tx,
                    case,
                    &hold.original_operation,
                    observed,
                    hold.received_at,
                )
                .await?;
            }
        }
        Ok(true)
    }

    /// A prior rejection can close the product case while A still owes a result.
    /// Retain the original's terminal receipt and alert without changing the decision.
    pub async fn closed_screen_lookup(
        &self,
        case_id: &str,
        operation: &str,
        outcome: ScreenOutcome,
        observed: DateTime<Utc>,
        received: DateTime<Utc>,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        let row:Option<(Json<ProductCase>,Json<ScreenObligation>)>=sqlx::query_as(
            "SELECT r.assessment,h.receipt FROM product_case c JOIN product_case_revision r USING(case_id,version)
             JOIN strict_screen_obligation h USING(case_id) WHERE c.case_id=$1 AND c.state IN ('APPROVED','REJECTED')
             AND h.original_operation=$2 FOR UPDATE OF c,h")
            .bind(case_id).bind(operation).fetch_optional(&mut *tx).await?;
        if let Some((Json(mut case), Json(hold))) = row {
            if hold.outcome.is_none() {
                let version = case.revision;
                case.screen_obligations.insert(operation.to_owned(), hold);
                case.record_screen_lookup(operation, outcome, observed, received, received)
                    .map_err(Error::Protocol)?;
                let saved = &case.screen_obligations[operation];
                super::strict_screen::write_obligation(&mut tx, case_id, saved).await?;
                if saved.outcome == Some(ScreenOutcome::Hit) {
                    case.revision = version;
                    enqueue_screen_alert(&mut tx, &case, operation, observed, Some(received))
                        .await?;
                }
            }
        }
        tx.commit().await
    }

    pub async fn late_product_screen(
        &self,
        case: &ProductCase,
        operation_id: &str,
        observed_at: DateTime<Utc>,
        received_at: Option<DateTime<Utc>>,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        enqueue_screen_alert(&mut tx, case, operation_id, observed_at, received_at).await?;
        tx.commit().await
    }
}

async fn enqueue_screen_alert(
    tx: &mut Transaction<'_, Postgres>,
    case: &ProductCase,
    operation_id: &str,
    observed_at: DateTime<Utc>,
    received_at: Option<DateTime<Utc>>,
) -> Result<(), Error> {
    let id = format!("{}:alert:{operation_id}", case.case_id);
    let event = json!({"event_id":id,"event_type":"ekyc.screening.alert.v2","case_id":case.case_id,
            "institution":case.tenant_id,"product":case.policy.product,"operation_id":operation_id,
            "observed_at":observed_at,"received_at":received_at,"reason":"POTENTIAL_SANCTIONS_MATCH","next_action":"investigate"});
    enqueue(tx, &id, &case.case_id, case.revision, event).await
}

async fn enqueue(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    case_id: &str,
    version: i64,
    payload: Value,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO decision_outbox(event_id,product_case_id,version,payload) VALUES($1,$2,$3,$4)
                 ON CONFLICT(event_id) DO NOTHING",
    )
    .bind(id)
    .bind(case_id)
    .bind(version)
    .bind(Json(payload))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

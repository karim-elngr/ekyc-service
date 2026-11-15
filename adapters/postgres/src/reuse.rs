use crate::Store;
use chrono::{DateTime, Datelike, Duration, Utc};
use ekyc_domain::product::{BindingSnapshot, Fact, Origin, Policy, ProductCase};
use ekyc_domain::CheckKind;
use sqlx::{types::Json, Error, PgConnection};
use std::collections::BTreeMap;

/// Source selection is performed by the same domain rules used at decision time.
/// Opening needs the newest applicable fact; later checks need each pinned fact
/// and at most one contrary observation since that original.
pub struct ReuseQuery {
    pub case_id: String,
    pub institution: String,
    pub subject: String,
    pub policy: Policy,
    pub bindings: BindingSnapshot,
    pub now: DateTime<Utc>,
    pub pinned: Option<BTreeMap<CheckKind, Fact>>,
}

impl ReuseQuery {
    pub fn pinned(case: &ProductCase, now: DateTime<Utc>) -> Self {
        let pinned = case
            .checks
            .values()
            .filter(|c| c.origin == Some(Origin::Reused))
            .filter_map(|c| c.fact.clone().map(|fact| (c.kind, fact)))
            .collect();
        Self {
            case_id: case.case_id.clone(),
            institution: case.tenant_id.clone(),
            subject: case.subject_ref.clone(),
            policy: case.policy.clone(),
            bindings: case.bindings.clone(),
            now,
            pinned: Some(pinned),
        }
    }

    pub(super) async fn select(&self, conn: &mut PgConnection) -> Result<Vec<Fact>, Error> {
        let mut facts = Vec::new();
        for rule in &self.policy.checks {
            let Some(reuse) = &rule.reuse else { continue };
            let original = self.pinned.as_ref().and_then(|map| map.get(&rule.kind));
            if self.pinned.is_some() && original.is_none() {
                continue;
            }
            let Some(doc) = &self.bindings.current_document_revision else {
                continue;
            };
            if let Some(pinned) = original {
                let stored = sqlx::query_scalar::<_,Json<Fact>>(
                    "SELECT fact FROM product_fact WHERE operation_id=$1 AND institution=$2 AND subject_ref=$3")
                    .bind(&pinned.operation_id).bind(&self.institution).bind(&self.subject)
                    .fetch_optional(&mut *conn).await?;
                if let Some(Json(fact)) = stored {
                    facts.push(fact);
                }
            }
            let start = self
                .now
                .checked_sub_signed(Duration::days(i64::from(reuse.max_age_days)))
                .filter(|t| t.year() > -4713);
            let start = match (start, original) {
                (Some(time), Some(fact)) => Some(time.max(fact.observed_at)),
                (None, Some(fact)) => Some(fact.observed_at),
                (time, None) => time,
            };
            // PostgreSQL timestamps have microsecond precision. Widen the SQL
            // bounds and let the domain compare the original nanoseconds.
            let mut cursor: Option<(DateTime<Utc>, String)> = None;
            let mut latest: Option<Fact> = None;
            // Seek in stable, bounded pages. Equal-time observations can span
            // pages; never discard a tie until the original timestamp is compared.
            'pages: loop {
                let rows = sqlx::query_as::<_,(DateTime<Utc>,String,Json<Fact>)>(
                    "SELECT observed_at,operation_id,fact FROM product_fact WHERE institution=$1 AND subject_ref=$2
                     AND fact#>>'{evidence,kind}'=$3 AND fact#>>'{evidence,document_revision}'=$4
                     AND observed_at >= COALESCE($5::timestamptz - interval '1 microsecond','-infinity'::timestamptz)
                     AND observed_at <= $6::timestamptz + interval '1 microsecond'
                     AND origin_case_id <> $7 AND fact->>'source_product'=ANY($8)
                     AND fact#>>'{evidence,method}'=ANY($9)
                     AND ($10::timestamptz IS NULL OR (observed_at,operation_id)<($10,$11))
                     ORDER BY observed_at DESC, operation_id DESC LIMIT 64")
                    .bind(&self.institution).bind(&self.subject).bind(rule.kind.as_str()).bind(doc)
                    .bind(start).bind(self.now).bind(&self.case_id).bind(&reuse.source_products)
                    .bind(&rule.methods).bind(cursor.as_ref().map(|v| v.0))
                    .bind(cursor.as_ref().map(|v| v.1.as_str())).fetch_all(&mut *conn).await?;
                let more = rows.len() == 64;
                for (time, operation, Json(fact)) in rows {
                    cursor = Some((time, operation));
                    if latest.as_ref().is_some_and(|best| {
                        best.observed_at.signed_duration_since(fact.observed_at)
                            > Duration::microseconds(1)
                    }) {
                        break 'pages;
                    }
                    if !rule.matches_candidate(
                        &fact,
                        &self.case_id,
                        &self.institution,
                        &self.subject,
                        &self.bindings,
                        self.now,
                    ) {
                        continue;
                    }
                    if let Some(original) = original {
                        if fact.observed_at >= original.observed_at
                            && !rule.accepts_source(&fact, self.now)
                        {
                            facts.push(fact);
                            break 'pages;
                        }
                    } else if latest
                        .as_ref()
                        .is_none_or(|best| rule.compare_candidates(&fact, best, self.now).is_gt())
                    {
                        latest = Some(fact);
                    }
                }
                if !more {
                    break;
                }
            }
            facts.extend(latest);
        }
        Ok(facts)
    }
}

impl Store {
    pub async fn reuse_candidates(&self, query: &ReuseQuery) -> Result<Vec<Fact>, Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let facts = query.select(&mut tx).await?;
        tx.commit().await?;
        Ok(facts)
    }
}

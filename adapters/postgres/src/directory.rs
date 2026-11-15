//! Scoped reads over the same transactionally maintained projection used for decisions.
use crate::{Error, Store};
use chrono::{DateTime, Utc};
use ekyc_domain::product::{CaseState, ProductCase};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{types::Json, Postgres, QueryBuilder};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct CaseScope {
    pub institution: String,
    pub products: Vec<String>,
    /// Product integrations own their cases. Reviewers and observers have an explicit product grant.
    pub owner: Option<String>,
}

pub struct CaseFilter {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub state: Option<String>,
    pub use_case: Option<String>,
    pub query: Option<String>,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageOrder {
    #[default]
    CreatedAtDesc,
    CreatedAtAsc,
}

#[derive(Serialize)]
pub struct CasePage {
    pub cases: Vec<Value>,
    pub total: i64,
}

#[derive(Serialize, Deserialize)]
pub struct CheckCount {
    pub kind: String,
    pub status: String,
    pub origin: Option<String>,
    pub count: i64,
}

#[derive(Serialize, Deserialize)]
pub struct CaseOverview {
    pub total: i64,
    pub states: BTreeMap<String, i64>,
    pub checks: Vec<CheckCount>,
}

#[derive(Serialize)]
pub struct Revision {
    pub version: i64,
    pub recorded_at: DateTime<Utc>,
    pub view: Value,
}

#[derive(Serialize)]
pub struct HistoryPage {
    pub revisions: Vec<Revision>,
    pub next_before_version: Option<i64>,
}

fn constrain_scope(sql: &mut QueryBuilder<Postgres>, scope: &CaseScope) {
    sql.push(" c.institution = ")
        .push_bind(scope.institution.clone())
        .push(" AND c.product = ANY(")
        .push_bind(scope.products.clone())
        .push(")");
    if let Some(owner) = &scope.owner {
        sql.push(" AND c.owner_id = ").push_bind(owner.clone());
    }
}

fn filtered<'a>(
    prefix: &'static str,
    scope: &CaseScope,
    filter: &CaseFilter,
) -> QueryBuilder<'a, Postgres> {
    let mut sql = QueryBuilder::new(prefix);
    sql.push(" FROM product_case c WHERE ");
    constrain_scope(&mut sql, scope);
    sql.push(" AND c.created_at >= ")
        .push_bind(filter.from)
        .push(" AND c.created_at < ")
        .push_bind(filter.to);
    if let Some(state) = &filter.state {
        sql.push(" AND c.state = ").push_bind(state.clone());
    }
    if let Some(use_case) = &filter.use_case {
        sql.push(" AND c.public_view->>'use_case' = ")
            .push_bind(use_case.clone());
    }
    if let Some(q) = &filter.query {
        // strpos searches literal text, including SQL wildcard characters.
        sql.push(" AND (strpos(lower(c.case_id), lower(")
            .push_bind(q.clone())
            .push(")) > 0 OR strpos(lower(c.subject_ref), lower(")
            .push_bind(q.clone())
            .push(")) > 0)");
    }
    sql
}

impl Store {
    pub async fn scoped_case(&self, id: &str, scope: &CaseScope) -> Result<Option<Value>, Error> {
        let mut sql =
            QueryBuilder::new("SELECT public_view FROM product_case c WHERE c.case_id = ");
        sql.push_bind(id).push(" AND ");
        constrain_scope(&mut sql, scope);
        sql.build_query_scalar::<Json<Value>>()
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|v| v.0))
    }

    pub async fn case_page(
        &self,
        scope: &CaseScope,
        filter: &CaseFilter,
        order: PageOrder,
        limit: u32,
        offset: u32,
    ) -> Result<CasePage, Error> {
        let mut sql = filtered(
            "WITH filtered AS NOT MATERIALIZED (SELECT c.created_at, c.case_id, c.public_view",
            scope,
            filter,
        );
        let sort = match order {
            PageOrder::CreatedAtAsc => "created_at ASC, case_id ASC",
            PageOrder::CreatedAtDesc => "created_at DESC, case_id DESC",
        };
        // One statement keeps totals and rows on one snapshot, even past the end of the list.
        sql.push("), page AS (SELECT * FROM filtered ORDER BY ").push(sort)
            .push(" LIMIT ").push_bind(i64::from(limit)).push(" OFFSET ").push_bind(i64::from(offset))
            .push(") SELECT (SELECT count(*) FROM filtered), COALESCE(jsonb_agg(public_view ORDER BY ")
            .push(sort).push("), '[]'::jsonb) FROM page");
        let (total, Json(cases)) = sql
            .build_query_as::<(i64, Json<Vec<Value>>)>()
            .fetch_one(&self.pool)
            .await?;
        Ok(CasePage { cases, total })
    }

    pub async fn case_overview(
        &self,
        scope: &CaseScope,
        filter: &CaseFilter,
    ) -> Result<CaseOverview, Error> {
        let mut sql = filtered(
            "WITH filtered AS (SELECT c.state, c.public_view->'checks' AS checks",
            scope,
            filter,
        );
        sql.push("), state_counts AS (
              SELECT state, count(*) AS n FROM filtered GROUP BY state
            ), check_counts AS (
              SELECT item->>'kind' AS kind,item->>'status' AS status,item->>'origin' AS origin,count(*) AS count
              FROM filtered CROSS JOIN LATERAL jsonb_array_elements(checks) item GROUP BY 1,2,3
            ) SELECT jsonb_build_object(
                'total', COALESCE((SELECT sum(n) FROM state_counts),0),
                'states', COALESCE((SELECT jsonb_object_agg(state,n) FROM state_counts),'{}'::jsonb),
                'checks', COALESCE((SELECT jsonb_agg(to_jsonb(check_counts) ORDER BY kind,status,origin) FROM check_counts),'[]'::jsonb))");
        let Json(mut result) = sql
            .build_query_scalar::<Json<CaseOverview>>()
            .fetch_one(&self.pool)
            .await?;
        for state in CaseState::ALL {
            result.states.entry(state.as_str().into()).or_insert(0);
        }
        Ok(result)
    }

    pub async fn case_history(
        &self,
        id: &str,
        scope: &CaseScope,
        before: Option<i64>,
        window: Option<(DateTime<Utc>, DateTime<Utc>)>,
        limit: u32,
    ) -> Result<Option<HistoryPage>, Error> {
        let mut sql = QueryBuilder::new(
            "SELECT r.version,r.recorded_at,r.public_view,CASE WHEN r.public_view IS NULL THEN r.assessment END FROM product_case c LEFT JOIN LATERAL (
               SELECT version,recorded_at,public_view,assessment FROM product_case_revision WHERE case_id=c.case_id"
        );
        if let Some((from, to)) = window {
            sql.push(" AND recorded_at >= ")
                .push_bind(from)
                .push(" AND recorded_at < ")
                .push_bind(to);
        }
        if let Some(version) = before {
            sql.push(" AND version < ").push_bind(version);
        }
        sql.push(" ORDER BY version DESC LIMIT ")
            .push_bind(i64::from(limit) + 1)
            .push(") r ON true WHERE c.case_id = ")
            .push_bind(id)
            .push(" AND ");
        constrain_scope(&mut sql, scope);
        sql.push(" ORDER BY r.version DESC");
        let rows = sql
            .build_query_as::<(
                Option<i64>,
                Option<DateTime<Utc>>,
                Option<Json<Value>>,
                Option<Json<ProductCase>>,
            )>()
            .fetch_all(&self.pool)
            .await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let more = rows.len() > limit as usize;
        let revisions: Vec<_> = rows
            .into_iter()
            .take(limit as usize)
            .filter_map(|(version, recorded_at, public_view, assessment)| {
                Some(Revision {
                    version: version?,
                    recorded_at: recorded_at?,
                    view: public_view
                        .map(|v| v.0)
                        .or_else(|| assessment.map(|old| old.0.view()))?,
                })
            })
            .collect();
        let next_before_version = revisions.last().filter(|_| more).map(|r| r.version);
        Ok(Some(HistoryPage {
            revisions,
            next_before_version,
        }))
    }
}

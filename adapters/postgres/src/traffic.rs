use crate::Store;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{Error, Row};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrafficChange {
    pub product: String,
    pub provider: String,
    pub expected_version: i64,
    pub mode: String,
    pub share_cap: Option<i32>,
    pub duration_seconds: i32,
    pub reason: String,
}

impl Store {
    pub async fn set_traffic(
        &self,
        institution: &str,
        actor: &str,
        key: &str,
        change: &TrafficChange,
    ) -> Result<Option<Value>, Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(jsonb_build_array($1::text,$2::text,$3::text)::text,729))").bind(institution).bind(&change.product).bind(&change.provider).execute(&mut *tx).await?;
        let request = json!({"actor":actor,"change":change});
        let prior = sqlx::query("SELECT request, effective FROM provider_override_history WHERE institution=$1 AND product=$2 AND provider=$3 AND command_id=$4")
            .bind(institution).bind(&change.product).bind(&change.provider).bind(key).fetch_optional(&mut *tx).await?;
        if let Some(row) = prior {
            return Ok((row.get::<Value, _>("request") == request).then(|| row.get("effective")));
        }
        let previous: Option<Value> = sqlx::query_scalar("SELECT to_jsonb(o) FROM provider_routing_override o WHERE institution=$1 AND product=$2 AND provider=$3 FOR UPDATE")
            .bind(institution).bind(&change.product).bind(&change.provider).fetch_optional(&mut *tx).await?;
        if previous
            .as_ref()
            .and_then(|p| p["version"].as_i64())
            .unwrap_or(0)
            != change.expected_version
        {
            return Ok(None);
        }
        let effective: Value = sqlx::query_scalar("INSERT INTO provider_routing_override AS o (institution,product,provider,version,mode,share_cap,expires_at,actor,reason) VALUES ($1,$2,$3,$4,$5,$6,clock_timestamp()+make_interval(secs=>$7),$8,$9) ON CONFLICT(institution,product,provider) DO UPDATE SET version=excluded.version, mode=excluded.mode, share_cap=excluded.share_cap, expires_at=excluded.expires_at, actor=excluded.actor, reason=excluded.reason, changed_at=clock_timestamp() RETURNING to_jsonb(o)")
            .bind(institution).bind(&change.product).bind(&change.provider).bind(change.expected_version+1).bind(&change.mode).bind(change.share_cap)
            .bind(f64::from(change.duration_seconds)).bind(actor).bind(&change.reason).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO provider_override_history VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(institution)
            .bind(&change.product)
            .bind(&change.provider)
            .bind(change.expected_version + 1)
            .bind(key)
            .bind(request)
            .bind(previous)
            .bind(&effective)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(effective))
    }
    pub async fn traffic_report(
        &self,
        institution: &str,
        products: &[String],
        page: u32,
        case_id: Option<&str>,
    ) -> Result<Value, Error> {
        let overrides:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(o) || jsonb_build_object('active',expires_at>clock_timestamp()) FROM provider_routing_override o WHERE institution=$1 AND product=ANY($2) ORDER BY product,provider").bind(institution).bind(products).fetch_all(&self.pool).await?;
        let selections:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('case_id',case_id,'product',product,'provider',provider,'reason',reason,'operation_id',operation_id,'predecessor',predecessor,'created_at',created_at,'status',status,'possible_send',possible_send,'owner',owner,'policy_version',policy_version,'bucket',bucket,'candidates',candidates,'override_version',override_version,'deadline_ms',deadline_ms,'selection_seq',selection_seq) FROM provider_route_selection WHERE institution=$1 AND product=ANY($2) AND kind='address' AND ($3::text IS NULL OR case_id=$3) ORDER BY created_at DESC,operation_id LIMIT 100").bind(institution).bind(products).bind(case_id).fetch_all(&self.pool).await?;
        let share:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('provider',provider,'reason',reason,'assignments',count(*)) FROM provider_route_selection WHERE institution=$1 AND product=ANY($2) AND kind='address' GROUP BY provider,reason ORDER BY provider,reason").bind(institution).bind(products).fetch_all(&self.pool).await?;
        let placement: Vec<Value> = sqlx::query_scalar(
            "WITH counts AS (
                SELECT provider, count(*) AS assignments,
                    count(*) FILTER (WHERE possible_send) AS authorized
                FROM provider_route_selection
                WHERE institution=$1 AND product=ANY($2) AND kind='address' AND selection_seq=1
                GROUP BY provider)
            SELECT jsonb_build_object('provider', provider, 'assignments', assignments,
                'percent', round(100.0*assignments/sum(assignments) OVER (),1),
                'authorized', authorized) FROM counts ORDER BY provider",
        )
        .bind(institution)
        .bind(products)
        .fetch_all(&self.pool)
        .await?;
        let impact:Value=sqlx::query_scalar(
            "SELECT jsonb_build_object('outstanding', count(DISTINCT case_id) FILTER (WHERE status='outstanding'),
                'blocked', count(DISTINCT case_id) FILTER (WHERE status='blocked'),
                'rescued', count(DISTINCT case_id) FILTER (WHERE status='complete' AND reason IN ('outage_diversion','unsent_diversion','ambiguous_replacement')
                    AND EXISTS (SELECT 1 FROM product_case c WHERE c.case_id=provider_route_selection.case_id AND c.state='APPROVED')),
                'oldest_outstanding',min(created_at) FILTER (WHERE status='outstanding'), 'operations',count(*) FILTER (WHERE status IN ('outstanding','blocked'))) FROM provider_route_selection
                WHERE institution=$1 AND product=ANY($2) AND kind='address'").bind(institution).bind(products).fetch_one(&self.pool).await?;
        let calls:Vec<Value>=sqlx::query_scalar(
            "SELECT jsonb_build_object('provider',a.provider,'lane',a.lane,'attempts',count(*),'finished',count(a.finished_at))
            FROM provider_attempt a JOIN product_case c ON c.case_id=a.case_id
            WHERE c.institution=$1 AND c.product=ANY($2) AND a.kind='address'
            GROUP BY a.provider,a.lane ORDER BY a.provider,a.lane").bind(institution).bind(products).fetch_all(&self.pool).await?;
        let spend:Vec<Value>=sqlx::query_scalar(
            "SELECT jsonb_build_object('account',s.account,'currency',s.currency,'period',s.period_start,'status',s.status,
                'minor_units',sum(s.cost_minor)) FROM provider_spend_reservation s WHERE EXISTS(
                SELECT 1 FROM provider_attempt a JOIN product_case c USING(case_id)
                WHERE c.institution=$1 AND c.product=ANY($2) AND a.kind='address'
                AND a.billing_account=s.account AND a.billable_unit=s.unit)
            GROUP BY s.account,s.currency,s.period_start,s.status ORDER BY s.period_start DESC,s.account,s.status")
            .bind(institution).bind(products).fetch_all(&self.pool).await?;
        let outstanding:Vec<Value>=sqlx::query_scalar(
            "SELECT jsonb_build_object('case_id',case_id,'operation_id',operation_id,'provider',provider,'product',product,
                'status',status,'owner',owner,'reason',reason,'since',created_at)
            FROM provider_route_selection WHERE institution=$1 AND product=ANY($2) AND kind='address' AND status IN ('outstanding','blocked')
            ORDER BY created_at,operation_id LIMIT 50 OFFSET $3").bind(institution).bind(products).bind(i64::from(page-1)*50).fetch_all(&self.pool).await?;
        let recovery: Vec<Value> = sqlx::query_scalar(
            "SELECT to_jsonb(h)-'institution' FROM provider_route_health h
            WHERE institution=$1 AND product=ANY($2) ORDER BY product,provider",
        )
        .bind(institution)
        .bind(products)
        .fetch_all(&self.pool)
        .await?;
        Ok(
            json!({"overrides":overrides,"recent_assignments":selections,"share":share,"placement":placement,"calls":calls,
            "impact":impact,"outstanding":outstanding,"spend":spend,"recovery":recovery}),
        )
    }
}

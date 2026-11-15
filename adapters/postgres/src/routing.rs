//! Atomic, idempotent placement before a worker can submit. Locators are kept off the reporting surface.
use super::{Error, Json, Store};
use chrono::{DateTime, Utc};
use ekyc_domain::{
    product::{BoundEvidence, ProductCase, RouteAvailability, RoutedAnswer, Rule},
    CheckKind, Observation,
};
use sqlx::{Postgres, Transaction};
use std::collections::BTreeMap;

type Tx<'a> = Transaction<'a, Postgres>;
type Prior = (String, String, i64, i32, bool, String);

impl Store {
    /// Use the same eligibility, breaker/override and recovery calculations in a read-only snapshot.
    /// This entry point does not initialize the database or take an admission token.
    pub async fn shadow_address(
        pool: &sqlx::PgPool,
        case: &ProductCase,
    ) -> Result<serde_json::Value, Error> {
        let kind = CheckKind::Address;
        let rule = case
            .policy
            .checks
            .iter()
            .find(|r| r.kind == kind)
            .ok_or_else(|| Error::Protocol("missing address rule".into()))?;
        let routing = rule
            .routing
            .as_ref()
            .ok_or_else(|| Error::Protocol("no address routing policy".into()))?;
        let anchor = case
            .checks
            .get(&kind)
            .and_then(|c| c.evidence.as_ref())
            .ok_or_else(|| Error::Protocol("missing bound address".into()))?;
        let eligible: Vec<_> = eligible_bindings(case, rule, anchor).into_keys().collect();
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let mut limits = BTreeMap::new();
        for provider in &eligible {
            limits.insert(
                provider.clone(),
                availability(&mut tx, case, provider, kind, false).await?.0,
            );
        }
        let bucket = routing.bucket(&anchor.requirement_id(&case.case_id));
        let choice = routing.select(&eligible, &limits, bucket);
        tx.commit().await?;
        Ok(
            serde_json::json!({"policy_version":case.policy.version,"eligible":eligible,"bucket":bucket,
            "restrictions":limits,"prospective_provider":choice.as_ref().map(|c|c.provider),
            "reason":choice.as_ref().map(|c|c.reason)}),
        )
    }

    /// The operation ID comes from the exclusive case writer. Retrying this transaction
    /// returns the first committed choice, even if the breaker or override has changed.
    pub async fn assign_route(
        &self,
        case: &ProductCase,
        kind: CheckKind,
        deadline_ms: i64,
        replacing: bool,
    ) -> Result<Option<(BoundEvidence, i64)>, Error> {
        let check = &case.checks[&kind];
        let operation = check
            .operation_id
            .as_ref()
            .ok_or_else(|| Error::Protocol("missing operation".into()))?;
        let anchor = check
            .evidence
            .as_ref()
            .ok_or_else(|| Error::Protocol("missing binding".into()))?;
        let requirement = anchor.requirement_id(&case.case_id);
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,1934))")
            .bind(&requirement)
            .execute(&mut *tx)
            .await?;
        if let Some((Json(binding),deadline)) = sqlx::query_as::<_,(Json<BoundEvidence>,i64)>(
            "SELECT b.binding,s.deadline_ms FROM provider_route_selection s JOIN provider_route_binding b
             USING(requirement_id,provider) WHERE s.operation_id=$1").bind(operation).fetch_optional(&mut *tx).await? {
            tx.commit().await?;
            return Ok(Some((binding,deadline)));
        }
        let rule = case
            .policy
            .checks
            .iter()
            .find(|r| r.kind == kind)
            .expect("pinned rule");
        let routing = rule.routing.as_ref();
        let approved = eligible_bindings(case, rule, anchor);
        let prior:Option<Prior> = sqlx::query_as("SELECT operation_id,provider,deadline_ms,selection_seq,possible_send,status
            FROM provider_route_selection WHERE requirement_id=$1 ORDER BY selection_seq DESC LIMIT 1 FOR UPDATE")
            .bind(&requirement).fetch_optional(&mut *tx).await?;
        let eligible: Vec<_> = approved.keys().cloned().collect();
        let deadline = prior.as_ref().map_or(deadline_ms, |p| p.2);
        let mut version = 0;
        let mut snapshot = BTreeMap::new();
        let bucket = routing.map(|r| i32::from(r.bucket(&requirement)));
        let choice = if let Some(r) = routing {
            let mut limits = BTreeMap::new();
            for vendor in &eligible {
                let (availability, rev) = availability(&mut tx, case, vendor, kind, true).await?;
                version = version.max(rev);
                snapshot.insert(vendor.clone(),serde_json::json!({"override_version":rev,
                    "share_cap":availability.share,"blocked":availability.blocked,"probe":availability.probe,
                    "probe_generation":availability.probe_generation,
                    "ordinary_share": if eligible.len()==1 || r.mode==ekyc_domain::product::PlacementMode::Priority {100}
                        else {r.roster.iter().find(|c|&c.provider==vendor).expect("approved vendor").weight}}));
                limits.insert(vendor.clone(), availability);
            }
            if let Some((_, provider, _, _, possible, status)) = &prior {
                if !replacing
                    || !matches!(status.as_str(), "assigned" | "outstanding")
                    || (*possible && (!r.replace_ambiguous || provider != "address-a"))
                {
                    return Ok(None);
                }
                limits.entry(provider.clone()).or_default().blocked = true;
            }
            r.select(&eligible, &limits, r.bucket(&requirement))
                .map(|c| (c.provider.to_owned(), c.reason))
                .or_else(|| {
                    if prior.is_none() {
                        r.select(&eligible, &BTreeMap::new(), r.bucket(&requirement))
                            .map(|c| (c.provider.to_owned(), "waiting_for_eligible_route"))
                    } else {
                        None
                    }
                })
        } else {
            approved
                .contains_key(&anchor.provider)
                .then(|| (anchor.provider.clone(), "priority"))
        };
        let Some((provider, reason)) = choice else {
            return Ok(None);
        };
        let (predecessor, seq, reason) = match &prior {
            Some((op, _, _, n, possible, _)) => {
                sqlx::query("UPDATE provider_route_selection SET retired=true, status=CASE WHEN status='assigned'
                             THEN 'retired' ELSE status END WHERE operation_id=$1")
                    .bind(op).execute(&mut *tx).await?;
                (
                    Some(op),
                    n + 1,
                    if *possible {
                        "ambiguous_replacement"
                    } else {
                        "unsent_diversion"
                    },
                )
            }
            None => (None, 1, reason),
        };
        for (vendor, binding) in &approved {
            sqlx::query(
                "INSERT INTO provider_route_binding VALUES($1,$2,$3) ON CONFLICT DO NOTHING",
            )
            .bind(&requirement)
            .bind(vendor)
            .bind(Json(binding))
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("INSERT INTO provider_route_selection(operation_id,case_id,institution,product,kind,
            requirement_id,evidence_revision,selection_seq,policy_version,candidates,bucket,provider,reason,override_version,
            predecessor,deadline_ms,placement_snapshot) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)")
            .bind(operation).bind(&case.case_id).bind(&case.tenant_id).bind(&case.policy.product).bind(kind.as_str())
            .bind(&requirement).bind(&anchor.revision).bind(seq).bind(&case.policy.version).bind(&eligible)
            .bind(bucket).bind(&provider).bind(reason).bind(version).bind(predecessor).bind(deadline).bind(Json(snapshot))
            .execute(&mut *tx).await?;
        let Json(binding) = sqlx::query_scalar::<_, Json<BoundEvidence>>(
            "SELECT binding FROM provider_route_binding WHERE requirement_id=$1 AND provider=$2",
        )
        .bind(&requirement)
        .bind(provider)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some((binding, deadline)))
    }

    pub async fn block_unsent_route(
        &self,
        case: &str,
        op: &str,
        reason: &str,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        // Sender reservations acquire this same case row first. An uncertain call
        // must stay recoverable; an expired-before-HTTP authorization has no wire effect.
        sqlx::query("SELECT 1 FROM product_case WHERE case_id=$1 FOR UPDATE")
            .bind(case)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE provider_route_selection SET status='blocked',block_reason=$3,possible_send=false
            WHERE case_id=$1 AND operation_id=$2 AND (status='assigned' OR (status='outstanding' AND
             EXISTS (SELECT FROM provider_attempt WHERE operation_id=$2 AND lane='submit' AND class='expired')
             AND NOT EXISTS (SELECT FROM provider_attempt WHERE operation_id=$2 AND lane='submit' AND class<>'expired')))")
            .bind(case).bind(op).bind(reason).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn finish_route(
        &self,
        case_id: &str,
        op: &str,
        status: &str,
        result: Option<&Observation>,
        observed: DateTime<Utc>,
        received: DateTime<Utc>,
    ) -> Result<(), Error> {
        sqlx::query("UPDATE provider_route_selection SET status=$2,result=$3,observed_at=$4,received_at=$5
            WHERE operation_id=$1 AND case_id=$6 AND possible_send AND status='outstanding' AND $2 IN ('complete','inconclusive','not_accepted')")
            .bind(op).bind(status).bind(result.map(Json)).bind(observed).bind(received).bind(case_id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn route_answers(&self, case_id: &str, op: &str) -> Result<Vec<RoutedAnswer>, Error> {
        let rows=sqlx::query_scalar::<_,Json<RoutedAnswer>>("SELECT jsonb_build_object(
            'operation_id',s.operation_id,'status',s.status,'binding',b.binding,
            'observation',s.result,'observed_at',s.observed_at,'received_at',s.received_at)
            FROM provider_route_selection s JOIN provider_route_binding b USING(requirement_id,provider)
            JOIN provider_route_selection current ON current.requirement_id=s.requirement_id
            WHERE current.operation_id=$1 AND current.case_id=$2 ORDER BY s.selection_seq")
            .bind(op).bind(case_id).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }
}

fn eligible_bindings(
    case: &ProductCase,
    rule: &Rule,
    anchor: &BoundEvidence,
) -> BTreeMap<String, BoundEvidence> {
    let kind = rule.kind;
    let routing = rule.routing.as_ref();
    let candidates = case
        .bindings
        .provider_candidates
        .get(&kind)
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(anchor));
    let mut approved = BTreeMap::new();
    for binding in candidates {
        if binding.tenant_id == case.tenant_id
            && binding.subject_ref == case.subject_ref
            && binding.kind == kind
            && binding.same_capture(anchor)
            && rule.methods.contains(&binding.method)
            && ekyc_domain::valid_reference(&binding.provider_ref)
            && routing.map_or(
                matches!(binding.provider.as_str(), "sanctions-a" | "sanctions-b"),
                |r| r.approves(binding),
            )
        {
            approved.insert(binding.provider.clone(), binding.clone());
        }
    }
    approved
}

async fn availability(
    tx: &mut Tx<'_>,
    case: &ProductCase,
    provider: &str,
    kind: CheckKind,
    persist: bool,
) -> Result<(RouteAvailability, i64), Error> {
    if persist {
        // Fence the first override insert as well as modifications of an existing row.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(jsonb_build_array($1::text,$2::text,$3::text)::text,729))")
        .bind(&case.tenant_id).bind(&case.policy.product).bind(provider).execute(&mut **tx).await?;
    }
    let override_row:Option<(String,Option<i32>,i64)> = sqlx::query_as(
        "SELECT mode,share_cap,version FROM provider_routing_override WHERE institution=$1 AND product=$2
         AND provider=$3 AND expires_at>clock_timestamp()")
        .bind(&case.tenant_id).bind(&case.policy.product).bind(provider).fetch_optional(&mut **tx).await?;
    let (mode, cap, version) = override_row.unwrap_or(("normal".into(), None, 0));
    let circuit:Option<(String, bool,bool,bool,i64)> = sqlx::query_as(
        "SELECT c.breaker_key,COALESCE(b.open_until>clock_timestamp(),false),b.open_until IS NOT NULL,
            COALESCE(b.probe_until>clock_timestamp(),false),b.generation FROM provider_route_contract c
            LEFT JOIN provider_circuit b USING(breaker_key) WHERE c.kind=$1 AND c.provider=$2")
        .bind(kind.as_str()).bind(provider).fetch_optional(&mut **tx).await?;
    let Some((breaker, open, was_open, probe_claimed, generation)) = circuit else {
        return Ok((
            RouteAvailability {
                blocked: true,
                ..Default::default()
            },
            version,
        ));
    };
    let health = if provider == "address-a" {
        Some(
            recovery_share(
                tx,
                case,
                provider,
                &breaker,
                (&mode, version),
                was_open,
                persist,
            )
            .await?,
        )
    } else {
        None
    };
    let share = match (cap.map(|n| n as u8), health.filter(|n| *n < 80)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    Ok((
        RouteAvailability {
            blocked: open || probe_claimed || matches!(mode.as_str(), "paused" | "draining"),
            probe: was_open && !open && !probe_claimed,
            probe_generation: was_open.then_some(generation),
            share,
        },
        version,
    ))
}

async fn recovery_share(
    tx: &mut Tx<'_>,
    case: &ProductCase,
    provider: &str,
    breaker: &str,
    override_state: (&str, i64),
    was_open: bool,
    persist: bool,
) -> Result<u8, Error> {
    let (mode, version) = override_state;
    if persist {
        sqlx::query("INSERT INTO provider_route_health(institution,product,provider) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(&case.tenant_id).bind(&case.policy.product).bind(provider).execute(&mut **tx).await?;
    }
    let (stage,cause,previous,since):(i32,String,i64,DateTime<Utc>)=sqlx::query_as(
        "SELECT coalesce(h.stage,80),coalesce(h.cause,'healthy'),coalesce(h.override_version,0),coalesce(h.since,clock_timestamp())
        FROM (VALUES(1)) singleton(x) LEFT JOIN provider_route_health h ON h.institution=$1 AND h.product=$2 AND h.provider=$3").bind(&case.tenant_id).bind(&case.policy.product).bind(provider).fetch_one(&mut **tx).await?;
    let (n,elapsed,p95,errors):(i32,f64,Option<f64>,Option<f64>)=sqlx::query_as(
        "SELECT count(*)::int, extract(epoch FROM clock_timestamp()-$2)::double precision,
        percentile_cont(.95) WITHIN GROUP(ORDER BY elapsed_ms),avg((class='transient')::int)::double precision
        FROM provider_attempt WHERE breaker_key=$1 AND lane='submit' AND finished_at >= greatest($2,clock_timestamp()-interval '60 seconds')
          AND class IN ('ok','transient','throttled','permanent')")
        .bind(breaker).bind(since).fetch_one(&mut **tx).await?;
    // Explicit synthetic thresholds: four calls and two seconds per stage. Only a new
    // completed window can advance; failures or high p95 step back immediately.
    let (next, why) = if was_open || (mode == "recovering" && previous != version) {
        (5, "recovering")
    } else if n >= 4 && (p95.is_some_and(|v| v > 450.0) || errors.is_some_and(|v| v > 0.20)) {
        (
            match stage {
                80 => 50,
                50 => 20,
                _ => 5,
            },
            "degraded",
        )
    } else if n >= 4
        && elapsed >= 2.0
        && p95.is_some_and(|v| v < 250.0)
        && errors == Some(0.0)
        && stage < 80
    {
        let target = match stage {
            5 => 20,
            20 => 50,
            _ => 80,
        };
        (
            target,
            if target == 80 {
                "healthy"
            } else {
                "recovering"
            },
        )
    } else {
        (stage, cause.as_str())
    };
    if !persist {
        return Ok(next as u8);
    }
    sqlx::query(
        "UPDATE provider_route_health SET stage=$4,cause=$5,override_version=$6,
        since=CASE WHEN stage<>$4 OR cause<>$5 THEN clock_timestamp() ELSE since END,
        samples=$7,p95_ms=$8,error_rate=$9,window_end=clock_timestamp()
        WHERE institution=$1 AND product=$2 AND provider=$3",
    )
    .bind(&case.tenant_id)
    .bind(&case.policy.product)
    .bind(provider)
    .bind(next)
    .bind(why)
    .bind(version)
    .bind(n)
    .bind(p95)
    .bind(errors)
    .execute(&mut **tx)
    .await?;
    Ok(next as u8)
}

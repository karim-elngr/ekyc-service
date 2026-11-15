//! Read a copied issuer binding and evaluate current PostgreSQL placement controls without sending.
use chrono::Utc;
use ekyc_domain::{
    product::{BindingSnapshot, BoundEvidence, Policy, ProductCase},
    CheckKind,
};
use ekyc_postgres::Store;
use serde::Deserialize;
use sqlx::PgPool;
use std::{collections::BTreeMap, error::Error, io};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sample {
    case_id: String,
    institution: String,
    subject_ref: String,
    use_case: String,
    observed_provider: String,
    asset: BoundEvidence,
    candidates: Vec<BoundEvidence>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let sample: Sample = serde_json::from_reader(io::stdin().lock())?;
    let kind = CheckKind::Address;
    let policy = Policy::load("wallet", &sample.use_case)?;
    let mut providers = std::collections::BTreeSet::new();
    if sample
        .candidates
        .iter()
        .any(|b| !sample.asset.same_capture(b) || !providers.insert(&b.provider))
    {
        return Err("invalid candidate identity".into());
    }
    let bindings = BindingSnapshot {
        provider_candidates: BTreeMap::from([(kind, sample.candidates)]),
        ..Default::default()
    };
    let case = ProductCase::open(
        sample.case_id,
        sample.institution,
        sample.subject_ref,
        policy,
        BTreeMap::from([(kind, sample.asset)]),
        &[],
        bindings,
        Utc::now(),
    )?;
    let pool = PgPool::connect(&std::env::var("DATABASE_URL")?).await?;
    let mut preview = Store::shadow_address(&pool, &case).await?;
    preview["observed_provider"] = sample.observed_provider.into();
    println!("{}", preview);
    Ok(())
}

mod case;
mod checks;
mod settings;

use crate::case::VerificationCase;
use crate::checks::ProviderChecks;
use crate::settings::{number, provider_limits, RetrySettings};
use ekyc_domain::Purpose;
use ekyc_http_transport::Transport;
use ekyc_ports::Provider;
use ekyc_postgres::Store;
use ekyc_provider_address::AddressProvider;
use ekyc_provider_document::DocumentProvider;
use ekyc_provider_face::FaceProvider;
use ekyc_provider_sanctions::SanctionsProvider;
use restate_sdk::prelude::{Endpoint, HttpServer};
use std::collections::BTreeMap;
use std::{env, error::Error, io, sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let bind = env::var("WORKER_BIND")
        .unwrap_or_else(|_| "0.0.0.0:9080".into())
        .parse()?;
    let store = Store::connect(&env::var("DATABASE_URL")?).await?;
    let timeout_ms = number("CHECK_TIMEOUT_MS", 2500, 120_000)?;
    let transport = Transport::new(Duration::from_millis(timeout_ms)).map_err(io::Error::other)?;
    let providers: Vec<Arc<dyn Provider>> = vec![
        Arc::new(DocumentProvider::new(
            transport.clone(),
            env::var("DOCUMENT_URL")?,
        )),
        Arc::new(FaceProvider::new(transport.clone(), env::var("FACE_URL")?)),
        Arc::new(AddressProvider::new(
            transport.clone(),
            env::var("ADDRESS_URL")?,
        )),
        Arc::new(SanctionsProvider::new(
            transport,
            env::var("SANCTIONS_URL")?,
        )),
    ];
    let limits = Purpose::FullOnboarding
        .checks()
        .iter()
        .map(|&k| provider_limits(k, timeout_ms).map(|limits| (k, limits)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let cases = VerificationCase {
        store: store.clone(),
        retry: RetrySettings::from_env()?,
    };
    let checks = ProviderChecks {
        store,
        providers: providers.into_iter().map(|p| (p.kind(), p)).collect(),
        limits,
    };
    let endpoint = Endpoint::builder().bind(cases).bind(checks).build();
    tracing::info!(%bind, "serving eKYC Restate endpoint");
    HttpServer::new(endpoint).listen_and_serve(bind).await;
    Ok(())
}

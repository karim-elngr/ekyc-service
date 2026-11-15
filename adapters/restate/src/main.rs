mod attempts;
mod checks;
mod checks_v2;
mod evidence;
mod interactive;
mod product;
mod providers;
mod rules;
mod settings;

use crate::checks::ProviderChecks;
use crate::interactive::InteractiveMocks;
use crate::settings::{number, provider_limits, RetrySettings};
use ekyc_domain::CheckKind;
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
    let mut providers: BTreeMap<(CheckKind, String), Arc<dyn Provider>> = BTreeMap::new();
    let mut registrations = BTreeMap::new();
    let catalog = providers::Catalog::from_env()?;
    for (&kind, routes) in &catalog.endpoints {
        for (id, base) in routes {
            if catalog.demo_callbacks && id == kind.as_str() {
                registrations.insert(
                    kind,
                    format!(
                        "{}/__mock__/interactive/register",
                        base.trim_end_matches('/')
                    ),
                );
            }
            let provider: Arc<dyn Provider> = match kind {
                CheckKind::Document => {
                    Arc::new(DocumentProvider::new(transport.clone(), base.clone()))
                }
                CheckKind::Face => Arc::new(FaceProvider::new(transport.clone(), base.clone())),
                CheckKind::Address => {
                    Arc::new(AddressProvider::new(transport.clone(), base.clone()))
                }
                CheckKind::Sanctions => {
                    Arc::new(SanctionsProvider::new(transport.clone(), base.clone()))
                }
            };
            providers.insert((kind, id.clone()), provider);
        }
    }
    let limits = CheckKind::ALL
        .into_iter()
        .map(|k| provider_limits(k, timeout_ms).map(|limits| (k, limits)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let rules = rules::Rules::from_env()?;
    for (kind, provider) in providers.keys() {
        if rules.pool(*kind, provider).is_none() {
            return Err(format!("missing contract for {}", provider).into());
        }
    }
    store.install_contracts(&rules.book).await?;
    let interactive = InteractiveMocks {
        transport,
        registrations,
    };
    let attempts = attempts::ProviderAttemptV2 {
        before_send_barrier: env::var_os("EKYC_DRILL_BEFORE_SEND_FILE").map(Into::into),
        store: store.clone(),
        providers: providers.clone(),
        interactive: interactive.clone(),
        rules: rules.clone(),
    };
    let products = product::ProductCase {
        rules: rules.clone(),
        store: store.clone(),
        resolver: evidence::EvidenceResolver::from_env()?,
        retry: RetrySettings::from_env()?,
        policies: product::Policies::from_env()?,
        catalog,
    };
    let checks = ProviderChecks {
        store,
        providers,
        interactive,
        rules,
        limits,
    };
    let endpoint = Endpoint::builder()
        .bind(checks)
        .bind(products)
        .bind(attempts)
        .build();
    tracing::info!(%bind, "serving eKYC Restate endpoint");
    HttpServer::new(endpoint).listen_and_serve(bind).await;
    Ok(())
}

mod auth;
mod directory;
mod interactive;
mod product;
mod staff;

mod server;

use auth::Clients;
use ekyc_postgres::Store;
use ekyc_restate_ingress::CaseCommands;
use server::{error, unavailable, App, Metrics, Shared};
use std::{env, error::Error, net::SocketAddr, sync::Arc, time::Duration};
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt().with_target(false).init();
    let clients = Clients::from_env()?;
    let store = Store::connect(&env::var("DATABASE_URL")?).await?;
    let ingress = env::var("RESTATE_INGRESS_URL")?.parse::<reqwest::Url>()?;
    if !matches!(ingress.scheme(), "http" | "https") || ingress.host().is_none() {
        return Err("invalid Restate ingress URL".into());
    }
    let commands = CaseCommands::new(ingress, Duration::from_secs(8))?;
    let limit = env::var("ADMISSION_PER_MINUTE")
        .unwrap_or_else(|_| "60".into())
        .parse()?;
    if limit == 0 {
        return Err("admission quota must be positive".into());
    }
    let state = Arc::new(App {
        store,
        commands,
        clients,
        interactive: interactive::MockProviders::from_env()?,
        limit,
        metrics: Metrics::default(),
    });
    let app = server::router(state);
    let addr: SocketAddr = env::var("API_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8090".into())
        .parse()?;
    let socket = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening");
    axum::serve(socket, app).await?;
    Ok(())
}

#[cfg(all(test, feature = "postgres-tests"))]
mod http_tests;

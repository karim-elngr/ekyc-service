use reqwest::{Client, Url};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::{env, error::Error, io, time::Duration};

pub(crate) struct Relay {
    pub url: Url,
    pub token: String,
    pub pool: PgPool,
    pub client: Client,
}

fn config(name: &str) -> io::Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required")))
}

pub(crate) fn load() -> Result<Relay, Box<dyn Error>> {
    let url = Url::parse(&config("WEBHOOK_URL")?)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid webhook endpoint").into());
    }
    let token = config("WEBHOOK_TOKEN")?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect_lazy(&config("DATABASE_URL")?)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()?;
    Ok(Relay {
        url,
        token,
        pool,
        client,
    })
}

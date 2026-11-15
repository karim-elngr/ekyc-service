//! Publish committed verification events to the configured webhook recipient.
mod delivery;
mod retry;
mod settings;

use std::{error::Error, time::Duration};
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let relay = settings::load()?;
    let mut idle_ms: u64 = 100;
    loop {
        match delivery::deliver(&relay.pool, &relay.client, &relay.url, &relay.token).await {
            Ok(true) => {
                idle_ms = 100;
                continue;
            }
            Err(_) => {
                eprintln!("webhook database unavailable; leaving outstanding deliveries for retry")
            }
            Ok(false) => {}
        }
        sleep(Duration::from_millis(idle_ms)).await;
        idle_ms = (idle_ms * 2).min(2000);
    }
}

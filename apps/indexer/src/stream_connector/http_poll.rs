use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::Value;
use tokio::time::sleep;

pub struct HttpPollConnector {
    endpoint: String,
    poll_interval: Duration,
}

impl HttpPollConnector {
    pub fn new(endpoint: String, poll_interval: Duration) -> Self {
        Self {
            endpoint,
            poll_interval,
        }
    }

    pub async fn connect(&mut self) -> Result<()> {
        Ok(())
    }

    pub async fn next_payload(&mut self) -> Result<Value> {
        sleep(self.poll_interval).await;
        Err(anyhow!(
            "http_poll connector is wired but not implemented yet for endpoint {}",
            self.endpoint
        ))
    }
}

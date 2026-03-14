use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use tokio::time::sleep;

pub struct HttpPollConnector {
    client: Client,
    endpoint: String,
    poll_interval: Duration,
}

impl HttpPollConnector {
    pub fn new(endpoint: String, poll_interval: Duration) -> Self {
        Self {
            client: Client::new(),
            endpoint,
            poll_interval,
        }
    }

    pub async fn connect(&mut self) -> Result<()> {
        Ok(())
    }

    pub async fn next_payload(&mut self) -> Result<Value> {
        sleep(self.poll_interval).await;
        let response = self
            .client
            .get(&self.endpoint)
            .send()
            .await
            .with_context(|| format!("http_poll request failed for {}", self.endpoint))?
            .error_for_status()
            .with_context(|| {
                format!(
                    "http_poll returned non-success status for {}",
                    self.endpoint
                )
            })?;
        response
            .json::<Value>()
            .await
            .with_context(|| format!("http_poll returned non-JSON payload for {}", self.endpoint))
    }
}

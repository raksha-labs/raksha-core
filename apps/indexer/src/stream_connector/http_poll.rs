use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

pub struct HttpPollConnector {
    client: Client,
    endpoint: String,
}

impl HttpPollConnector {
    pub fn new(endpoint: String, _poll_interval: std::time::Duration) -> Self {
        Self {
            client: Client::new(),
            endpoint,
        }
    }

    pub async fn connect(&mut self) -> Result<()> {
        Ok(())
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn fetch_payload(&self, endpoint: &str) -> Result<Option<Value>> {
        let response = self
            .client
            .get(endpoint)
            .send()
            .await
            .with_context(|| format!("http_poll request failed for {endpoint}"))?;
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let response = response
            .error_for_status()
            .with_context(|| format!("http_poll returned non-success status for {endpoint}"))?;
        response
            .json::<Value>()
            .await
            .map(Some)
            .with_context(|| format!("http_poll returned non-JSON payload for {endpoint}"))
    }
}

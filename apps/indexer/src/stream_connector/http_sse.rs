use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::{Client, Response};
use serde_json::Value;
use tokio::time::timeout;

pub struct HttpSseConnector {
    client: Client,
    endpoint: String,
    response: Option<Response>,
    line_buffer: Vec<u8>,
    pending_data_lines: Vec<String>,
}

impl HttpSseConnector {
    pub fn new(endpoint: String) -> Self {
        Self {
            client: Client::new(),
            endpoint,
            response: None,
            line_buffer: Vec::new(),
            pending_data_lines: Vec::new(),
        }
    }

    pub async fn connect(&mut self) -> Result<()> {
        let response = self
            .client
            .get(&self.endpoint)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .with_context(|| format!("http_sse request failed for {}", self.endpoint))?
            .error_for_status()
            .with_context(|| {
                format!("http_sse returned non-success status for {}", self.endpoint)
            })?;
        self.response = Some(response);
        self.line_buffer.clear();
        self.pending_data_lines.clear();
        Ok(())
    }

    pub async fn next_payload(&mut self) -> Result<Value> {
        loop {
            if let Some(payload) = self.try_parse_buffered_event()? {
                return Ok(payload);
            }

            let response = self
                .response
                .as_mut()
                .ok_or_else(|| anyhow!("http_sse stream is not connected"))?;
            let chunk = match timeout(Duration::from_secs(1), response.chunk()).await {
                Ok(result) => {
                    result.with_context(|| format!("http_sse read failed for {}", self.endpoint))?
                }
                Err(_) => continue,
            };
            let Some(chunk) = chunk else {
                return Err(anyhow!("http_sse stream ended"));
            };
            self.line_buffer.extend_from_slice(&chunk);
        }
    }

    fn try_parse_buffered_event(&mut self) -> Result<Option<Value>> {
        while let Some(line) = self.next_line() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                if self.pending_data_lines.is_empty() {
                    continue;
                }
                let raw_event = self.pending_data_lines.join("\n");
                self.pending_data_lines.clear();
                let payload = serde_json::from_str::<Value>(&raw_event)
                    .with_context(|| format!("invalid http_sse JSON payload: {raw_event}"))?;
                return Ok(Some(payload));
            }
            if line.starts_with(':') {
                continue;
            }
            if let Some(data) = line.strip_prefix("data:") {
                self.pending_data_lines.push(data.trim_start().to_string());
            }
        }
        Ok(None)
    }

    fn next_line(&mut self) -> Option<String> {
        let newline_idx = self.line_buffer.iter().position(|byte| *byte == b'\n')?;
        let line = self.line_buffer.drain(..=newline_idx).collect::<Vec<_>>();
        let line = &line[..line.len().saturating_sub(1)];
        Some(String::from_utf8_lossy(line).into_owned())
    }
}

/// Health check HTTP server for worker monitoring
///
/// Provides /health and /ready endpoints for Kubernetes
/// liveness and readiness probes.
use anyhow::Result;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::info;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HealthCommand {
    TriggerReload,
}

/// Health check status information
#[derive(Debug, Clone)]
pub struct HealthStatus {
    pub service_name: String,
    pub is_ready: bool,
    pub redis_connected: bool,
    pub postgres_connected: bool,
    pub details: Vec<String>,
}

impl Default for HealthStatus {
    fn default() -> Self {
        Self {
            service_name: "unknown".to_string(),
            is_ready: false,
            redis_connected: false,
            postgres_connected: false,
            details: Vec::new(),
        }
    }
}

/// Simple HTTP health check server
///
/// Runs in background and responds to:
/// - GET /health - Liveness probe (always returns 200)
/// - GET /ready - Readiness probe (returns 200 if ready, 503 if not)
/// - GET /metrics - Basic metrics (optional)
pub struct HealthCheckServer {
    addr: SocketAddr,
    status: Arc<tokio::sync::RwLock<HealthStatus>>,
    command_tx: Option<mpsc::Sender<HealthCommand>>,
    command_token: Option<String>,
}

impl HealthCheckServer {
    /// Create a new health check server
    pub fn new(port: u16, service_name: impl Into<String>) -> Self {
        let addr: SocketAddr = ([0, 0, 0, 0], port).into();
        Self {
            addr,
            status: Arc::new(tokio::sync::RwLock::new(HealthStatus {
                service_name: service_name.into(),
                ..Default::default()
            })),
            command_tx: None,
            command_token: None,
        }
    }

    pub fn with_command_channel(
        mut self,
        command_tx: mpsc::Sender<HealthCommand>,
        command_token: Option<String>,
    ) -> Self {
        self.command_tx = Some(command_tx);
        self.command_token = command_token.and_then(|value| {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });
        self
    }

    /// Get a handle to update health status
    pub fn status_handle(&self) -> Arc<tokio::sync::RwLock<HealthStatus>> {
        self.status.clone()
    }

    /// Start the health check server in the background
    pub fn start(self) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(async move { self.run().await })
    }

    async fn run(self) -> Result<()> {
        let listener = TcpListener::bind(self.addr).await?;
        info!("health check server listening on {}", self.addr);

        loop {
            let (mut socket, _) = listener.accept().await?;
            let status = self.status.clone();
            let command_tx = self.command_tx.clone();
            let command_token = self.command_token.clone();

            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];

                match socket.read(&mut buffer).await {
                    Ok(n) if n > 0 => {
                        let request = String::from_utf8_lossy(&buffer[..n]);
                        let request_line = request.lines().next().unwrap_or_default();

                        let response = if request_line.starts_with("GET /health") {
                            Self::health_response()
                        } else if request_line.starts_with("GET /ready") {
                            Self::ready_response(&status).await
                        } else if request_line.starts_with("GET /metrics") {
                            Self::metrics_response(&status).await
                        } else if request_line.starts_with("POST /reload") {
                            Self::reload_response(&request, command_tx, command_token).await
                        } else {
                            Self::not_found_response()
                        };

                        let _ = socket.write_all(response.as_bytes()).await;
                    }
                    _ => {}
                }
            });
        }
    }

    fn health_response() -> String {
        let body = json!({
            "status": "ok",
            "message": "service is alive"
        });

        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.to_string().len(),
            body
        )
    }

    async fn ready_response(status: &Arc<tokio::sync::RwLock<HealthStatus>>) -> String {
        let status = status.read().await;

        if status.is_ready {
            let body = json!({
                "status": "ready",
                "service": status.service_name,
                "redis": status.redis_connected,
                "postgres": status.postgres_connected,
                "details": status.details
            });

            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.to_string().len(),
                body
            )
        } else {
            let body = json!({
                "status": "not_ready",
                "service": status.service_name,
                "redis": status.redis_connected,
                "postgres": status.postgres_connected,
                "details": status.details
            });

            format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.to_string().len(),
                body
            )
        }
    }

    async fn metrics_response(status: &Arc<tokio::sync::RwLock<HealthStatus>>) -> String {
        let status = status.read().await;

        let body = json!({
            "service": status.service_name,
            "redis_connected": if status.redis_connected { 1 } else { 0 },
            "postgres_connected": if status.postgres_connected { 1 } else { 0 },
            "is_ready": if status.is_ready { 1 } else { 0 }
        });

        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.to_string().len(),
            body
        )
    }

    fn not_found_response() -> String {
        let body = json!({
            "error": "not found",
            "available_endpoints": ["/health", "/ready", "/metrics", "/reload"]
        });

        format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.to_string().len(),
            body
        )
    }

    async fn reload_response(
        request: &str,
        command_tx: Option<mpsc::Sender<HealthCommand>>,
        command_token: Option<String>,
    ) -> String {
        let Some(command_tx) = command_tx else {
            let body = json!({
                "error": "reload_not_supported",
            });
            return format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.to_string().len(),
                body
            );
        };

        if let Some(expected_token) = command_token {
            let provided_token = Self::request_header(request, "x-internal-service-token");
            if provided_token.as_deref() != Some(expected_token.as_str()) {
                let body = json!({
                    "error": "unauthorized",
                });
                return format!(
                    "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.to_string().len(),
                    body
                );
            }
        }

        if command_tx.send(HealthCommand::TriggerReload).await.is_err() {
            let body = json!({
                "error": "reload_unavailable",
            });
            return format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.to_string().len(),
                body
            );
        }

        let body = json!({
            "status": "accepted",
            "message": "reload requested",
        });
        format!(
            "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.to_string().len(),
            body
        )
    }

    fn request_header(request: &str, header_name: &str) -> Option<String> {
        let prefix = format!("{}:", header_name.to_ascii_lowercase());
        request
            .lines()
            .skip(1)
            .take_while(|line| !line.trim().is_empty())
            .find_map(|line| {
                let trimmed = line.trim();
                let lower = trimmed.to_ascii_lowercase();
                if !lower.starts_with(&prefix) {
                    return None;
                }
                trimmed
                    .split_once(':')
                    .map(|(_, value)| value.trim().to_string())
            })
    }
}

/// Helper to start health check server with defaults from environment
pub fn start_health_check_server(
    service_name: impl Into<String>,
) -> Option<Arc<tokio::sync::RwLock<HealthStatus>>> {
    let port = std::env::var("HEALTH_CHECK_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);

    let enabled = std::env::var("HEALTH_CHECK_ENABLED")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(true);

    if !enabled {
        info!("health check server disabled");
        return None;
    }

    let server = HealthCheckServer::new(port, service_name);
    let status = server.status_handle();

    server.start();

    Some(status)
}

pub fn start_health_check_server_with_commands(
    service_name: impl Into<String>,
    command_tx: mpsc::Sender<HealthCommand>,
    command_token: Option<String>,
) -> Option<Arc<tokio::sync::RwLock<HealthStatus>>> {
    let port = std::env::var("HEALTH_CHECK_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);

    let enabled = std::env::var("HEALTH_CHECK_ENABLED")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(true);

    if !enabled {
        info!("health check server disabled");
        return None;
    }

    let server =
        HealthCheckServer::new(port, service_name).with_command_channel(command_tx, command_token);
    let status = server.status_handle();

    server.start();

    Some(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_check_server() {
        let server = HealthCheckServer::new(0, "test-service");
        let status_handle = server.status_handle();

        // Update status
        {
            let mut status = status_handle.write().await;
            status.is_ready = true;
            status.redis_connected = true;
        }

        // Verify status
        let status = status_handle.read().await;
        assert!(status.is_ready);
        assert!(status.redis_connected);
    }
}

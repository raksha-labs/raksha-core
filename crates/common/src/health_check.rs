/// Health check HTTP server for worker monitoring
///
/// Provides /health and /ready endpoints for Kubernetes
/// liveness and readiness probes.
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Maximum size of an inbound JSON request body the server is willing to
/// accept on a registered handler route. Larger bodies are rejected with
/// 413. Sized so multi-case dry-run requests fit comfortably (~50 cases at
/// ~250 bytes each), with headroom.
const MAX_JSON_REQUEST_BYTES: usize = 64 * 1024;

/// Handler signature for JSON POST routes registered via
/// [`HealthCheckServer::with_json_handler`]. The handler is invoked with the
/// parsed request body and must return a JSON value to serialize back to
/// the caller. Errors should be encoded inside the returned value (e.g. an
/// `error` field) — the wire status is always 200 unless the handler
/// itself returns an empty `null` value (treated as a programming bug).
pub type JsonHandler = Arc<dyn Fn(Value) -> Value + Send + Sync>;

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
    /// Map of `POST` paths to JSON handler closures. Populated via
    /// [`Self::with_json_handler`]. Stored in an `Arc<HashMap>` so the
    /// per-connection task can clone it cheaply.
    json_handlers: Arc<HashMap<String, JsonHandler>>,
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
            json_handlers: Arc::new(HashMap::new()),
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

    /// Register a JSON `POST` handler at `path`. The handler receives the
    /// parsed request body as `serde_json::Value` and returns a JSON value
    /// to serialize back to the caller as a 200 response.
    ///
    /// Authentication: routes registered through this method are gated on
    /// the same `x-internal-service-token` header as `POST /reload` when
    /// the server was constructed with a `command_token`. When no token is
    /// configured (test/dev mode), the route is unauthenticated. The
    /// header check is enforced by the dispatcher in `run`.
    pub fn with_json_handler(mut self, path: impl Into<String>, handler: JsonHandler) -> Self {
        let mut handlers = (*self.json_handlers).clone();
        handlers.insert(path.into(), handler);
        self.json_handlers = Arc::new(handlers);
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
            let json_handlers = self.json_handlers.clone();

            tokio::spawn(async move {
                // Read up to MAX_JSON_REQUEST_BYTES from the socket. For
                // GETs / small POSTs the first read returns everything in a
                // single syscall. For larger JSON bodies we keep reading
                // until we've seen Content-Length bytes past the header
                // separator (or hit the size cap).
                let mut buffer = Vec::with_capacity(8 * 1024);
                let mut chunk = [0u8; 8 * 1024];
                let (header_end_idx, content_length) = loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) => return,
                        Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        Err(_) => return,
                    }
                    if buffer.len() > MAX_JSON_REQUEST_BYTES {
                        let body = json!({"error": "request_too_large"});
                        let _ = socket
                            .write_all(
                                Self::format_json_response("413 Payload Too Large", &body)
                                    .as_bytes(),
                            )
                            .await;
                        return;
                    }
                    if let Some(idx) = find_double_crlf(&buffer) {
                        let end = idx + 4;
                        let header_str = String::from_utf8_lossy(&buffer[..idx]);
                        let cl = header_value(&header_str, "content-length")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buffer.len() - end >= cl {
                            break (end, cl);
                        }
                    }
                };

                let header_section = String::from_utf8_lossy(&buffer[..header_end_idx]).to_string();
                let body_bytes = &buffer[header_end_idx..header_end_idx + content_length];
                let request_line = header_section
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();

                let response = if request_line.starts_with("GET /health") {
                    Self::health_response()
                } else if request_line.starts_with("GET /ready") {
                    Self::ready_response(&status).await
                } else if request_line.starts_with("GET /metrics") {
                    Self::metrics_response(&status).await
                } else if request_line.starts_with("POST /reload") {
                    Self::reload_response(&header_section, command_tx, command_token).await
                } else if request_line.starts_with("POST ") {
                    Self::dispatch_json_handler(
                        &request_line,
                        &header_section,
                        body_bytes,
                        &json_handlers,
                        &command_token,
                    )
                } else {
                    Self::not_found_response()
                };

                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    }

    /// Look up a registered JSON handler for the request line's path,
    /// invoke it with the parsed body, and serialize the response. Errors
    /// at every step (auth, parse, missing handler) are returned as JSON
    /// with an appropriate HTTP status. The handler closure itself is
    /// expected to encode domain errors inside its return value.
    fn dispatch_json_handler(
        request_line: &str,
        header_section: &str,
        body_bytes: &[u8],
        handlers: &HashMap<String, JsonHandler>,
        command_token: &Option<String>,
    ) -> String {
        let path = match request_line.split_whitespace().nth(1) {
            Some(p) => p.split('?').next().unwrap_or(p).to_string(),
            None => {
                return Self::format_json_response(
                    "400 Bad Request",
                    &json!({"error": "malformed_request_line"}),
                );
            }
        };
        let Some(handler) = handlers.get(&path) else {
            return Self::not_found_response();
        };

        // Token gating: a configured `command_token` protects every JSON
        // route too, the same way it protects /reload. Routes registered
        // without a configured token are open (test/dev mode).
        if let Some(expected) = command_token {
            let provided = Self::request_header(header_section, "x-internal-service-token");
            if provided.as_deref() != Some(expected.as_str()) {
                return Self::format_json_response(
                    "401 Unauthorized",
                    &json!({"error": "unauthorized"}),
                );
            }
        }

        let body_value: Value = if body_bytes.is_empty() {
            Value::Null
        } else {
            match serde_json::from_slice(body_bytes) {
                Ok(v) => v,
                Err(err) => {
                    warn!(
                        path = %path,
                        error = %err,
                        "json handler received malformed body"
                    );
                    return Self::format_json_response(
                        "400 Bad Request",
                        &json!({"error": "invalid_json", "detail": err.to_string()}),
                    );
                }
            }
        };

        let response_value = handler(body_value);
        Self::format_json_response("200 OK", &response_value)
    }

    fn format_json_response(status_line: &str, body: &Value) -> String {
        let body_str = body.to_string();
        format!(
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            status_line,
            body_str.len(),
            body_str
        )
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
        header_value(request, header_name)
    }
}

/// Find the first index of `\r\n\r\n` in the buffer (the end of the HTTP
/// header section). Returns `None` while the headers are still streaming
/// in. Used by the request loop to know when it's safe to start parsing.
fn find_double_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Case-insensitive header lookup over the raw header section. Returns the
/// trimmed value of the first matching header, or `None` if no header by
/// that name exists. Tolerant of either `\r\n` or `\n` line endings (the
/// hand-rolled server uses `\r\n`, but tests may inject `\n`).
fn header_value(request: &str, header_name: &str) -> Option<String> {
    let prefix = format!("{}:", header_name.to_ascii_lowercase());
    request
        .lines()
        .skip(1) // skip the request line
        .take_while(|line| !line.trim().is_empty())
        .find_map(|line| {
            let trimmed = line.trim();
            if !trimmed.to_ascii_lowercase().starts_with(&prefix) {
                return None;
            }
            trimmed
                .split_once(':')
                .map(|(_, value)| value.trim().to_string())
        })
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
    start_health_check_server_with_commands_and_handlers(
        service_name,
        command_tx,
        command_token,
        Vec::new(),
    )
}

/// Start the health-check server with both a reload command channel and a
/// list of JSON `POST` route handlers.
///
/// This is the entry point binaries use when they need to expose
/// service-internal RPC endpoints (e.g. the detector's
/// `/v1/depeg_v2/test_expression` dry-run route in Phase 3b). The handler
/// closures must be `Send + Sync + 'static` because they're invoked from
/// per-connection tokio tasks. They are gated on the same
/// `x-internal-service-token` header as `POST /reload`.
pub fn start_health_check_server_with_commands_and_handlers(
    service_name: impl Into<String>,
    command_tx: mpsc::Sender<HealthCommand>,
    command_token: Option<String>,
    json_handlers: Vec<(String, JsonHandler)>,
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

    let mut server =
        HealthCheckServer::new(port, service_name).with_command_channel(command_tx, command_token);
    for (path, handler) in json_handlers {
        server = server.with_json_handler(path, handler);
    }
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

use anyhow::Result;
use async_trait::async_trait;
use event_schema::{
    AlertEvent, DependencyEdge, DetectionResult, FeatureVector, FinalityUpdate, NormalizedEvent,
    ReorgNotice, RiskScore, RuleSimulationReport,
};
use serde_json::Value;
use tracing::error;

pub mod config;
pub use config::*;

pub mod shutdown;
pub use shutdown::ShutdownSignal;

pub mod circuit_breaker;
pub use circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitBreakerError, CircuitBreakerState,
};

pub mod health_check;
pub use health_check::{start_health_check_server, HealthCheckServer, HealthStatus};

pub mod data_source;
pub use data_source::DataSourceConfig;

pub mod postgres;
pub use postgres::{connect_postgres_client, make_postgres_tls_connector};

pub mod errors {
    use thiserror::Error;

    #[derive(Debug, Error)]
    pub enum CommonError {
        #[error("configuration error: {0}")]
        Config(String),
        #[error(transparent)]
        Other(#[from] anyhow::Error),
    }
}

pub fn init_logging(default_filter: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
    install_single_line_panic_hook();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .compact()
        .with_ansi(false)
        .init();
}

fn install_single_line_panic_hook() {
    std::panic::set_hook(Box::new(|panic_info| {
        let location = panic_info
            .location()
            .map(|location| format!("{}:{}:{}", location.file(), location.line(), location.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let payload = panic_info
            .payload()
            .downcast_ref::<&str>()
            .map(|value| (*value).to_string())
            .or_else(|| panic_info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".to_string());
        let backtrace = std::backtrace::Backtrace::force_capture();
        let backtrace_text = flatten_multiline(&format!("{backtrace:?}"));

        error!(
            target: "panic",
            location = %location,
            payload = %flatten_multiline(&payload),
            backtrace = %backtrace_text,
            "application panic"
        );
    }));
}

fn flatten_multiline(value: &str) -> String {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

pub fn event_id(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

#[async_trait]
pub trait ChainAdapter: Send + Sync {
    async fn next_events(&mut self) -> Result<Vec<NormalizedEvent>>;
    fn chain_name(&self) -> &'static str;

    async fn subscribe_heads(&mut self) -> Result<()> {
        Ok(())
    }

    async fn subscribe_logs(&mut self) -> Result<()> {
        Ok(())
    }

    async fn backfill_range(
        &mut self,
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<NormalizedEvent>> {
        Ok(Vec::new())
    }

    async fn latest_block_number(&mut self) -> Result<Option<u64>> {
        Ok(None)
    }

    fn chain_id(&self) -> Option<u64> {
        None
    }
}

#[async_trait]
pub trait RuleEvaluator: Send + Sync {
    async fn evaluate(&self, event: &NormalizedEvent) -> Result<DetectionResult>;

    async fn evaluate_window(
        &self,
        events: &[NormalizedEvent],
        _window_ms: u64,
    ) -> Result<Vec<DetectionResult>> {
        let mut out = Vec::new();
        for event in events {
            out.push(self.evaluate(event).await?);
        }
        Ok(out)
    }
}

#[async_trait]
pub trait RiskScorer: Send + Sync {
    async fn score(&self, result: &DetectionResult) -> Result<RiskScore>;
}

#[async_trait]
pub trait AlertSink: Send + Sync {
    async fn send(&self, alert: &AlertEvent) -> Result<()>;
    fn sink_name(&self) -> &'static str;
}

#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish_json(&self, stream: &str, payload: &Value) -> Result<()>;
    async fn healthcheck(&self) -> Result<()>;
}

#[async_trait]
pub trait FinalityEngine: Send + Sync {
    async fn apply_update(&self, update: &FinalityUpdate) -> Result<()>;
    async fn handle_reorg_notice(&self, notice: &ReorgNotice) -> Result<()>;
}

#[async_trait]
pub trait CorrelationEngine: Send + Sync {
    async fn correlate_detection(&self, detection: &DetectionResult) -> Result<Option<String>>;
    async fn dependency_edges_for_protocol(&self, protocol: &str) -> Result<Vec<DependencyEdge>>;
}

#[async_trait]
pub trait TenantPolicyResolver: Send + Sync {
    async fn resolve_alert_routes(
        &self,
        tenant_id: Option<&str>,
        detection: &DetectionResult,
    ) -> Result<Vec<String>>;
}

#[async_trait]
pub trait FeatureExtractor: Send + Sync {
    async fn extract(&self, detection: &DetectionResult) -> Result<FeatureVector>;
}

#[async_trait]
pub trait RuleSimulator: Send + Sync {
    async fn simulate(
        &self,
        tenant_id: Option<&str>,
        rule_id: &str,
        fixture_name: &str,
    ) -> Result<RuleSimulationReport>;
}

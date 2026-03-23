use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use common::make_postgres_tls_connector;
use futures_util::future::poll_fn;
use serde_json::json;
use sha2::{Digest, Sha256};
use state_manager::{
    EffectiveStreamConfig, PostgresRawRepository, PostgresRepository, RedisStreamPublisher,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::interval,
};
use tokio_postgres::AsyncMessage;
use tracing::{info, warn};
use url::Url;

use crate::stream_worker::{run_stream_worker, RuntimeStreamConfig};

// Keep notify-driven reloads fast, but avoid unnecessary full reconciles when
// stream configs are mostly stable. If LISTEN/NOTIFY is unavailable, fall back
// to a shorter resync so ephemeral simulation streams are still picked up.
const NOTIFY_CONNECTED_RECONCILE_INTERVAL_SECS: u64 = 600;
const NOTIFY_DEGRADED_RECONCILE_INTERVAL_SECS: u64 = 30;
const DEFAULT_PURGE_INTERVAL_SECS: u64 = 15;
const DEFAULT_RETENTION_SECONDS: i64 = 300;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NotifySignal {
    Connected,
    Changed,
    Disconnected,
}

struct WorkerHandle {
    config_hash: String,
    stop_tx: watch::Sender<bool>,
    join_handle: JoinHandle<()>,
}

pub async fn run_stream_supervisor(
    repo: PostgresRepository,
    stream: RedisStreamPublisher,
    database_url: String,
    purge_enabled: bool,
) -> Result<()> {
    let raw_repo = match PostgresRawRepository::from_env().await {
        Some(Ok(repo)) => Some(repo),
        Some(Err(error)) => {
            common::log_error!(
                warn,
                error,
                "RAW_DATABASE_URL configured but raw repository init failed; continuing without raw writer"
            );
            None
        }
        None => {
            warn!(
                "RAW_DATABASE_URL not set; stream supervisor using core operational storage only"
            );
            None
        }
    };

    let mut workers: HashMap<String, WorkerHandle> = HashMap::new();
    let mut listener_connected = false;
    let mut notify_rx = spawn_config_notify_listener(database_url);
    let mut reconcile_tick = build_reconcile_tick(listener_connected);
    let mut purge_tick = interval(Duration::from_secs(DEFAULT_PURGE_INTERVAL_SECS));

    reconcile(&repo, raw_repo.as_ref(), &stream, &mut workers).await?;

    loop {
        tokio::select! {
            _ = reconcile_tick.tick() => {
                if let Err(error) = reconcile(&repo, raw_repo.as_ref(), &stream, &mut workers).await {
                    common::log_error!(warn, error, "stream supervisor periodic reconcile failed");
                }
            }
            signal = notify_rx.recv() => {
                let mut should_reconcile = false;
                match signal {
                    Some(NotifySignal::Connected) => {
                        if !listener_connected {
                            listener_connected = true;
                            reconcile_tick = build_reconcile_tick(listener_connected);
                            info!(
                                reconcile_interval_secs = NOTIFY_CONNECTED_RECONCILE_INTERVAL_SECS,
                                "stream config notify listener active; using long periodic fallback"
                            );
                        }
                        should_reconcile = true;
                    }
                    Some(NotifySignal::Changed) => {
                        should_reconcile = true;
                    }
                    Some(NotifySignal::Disconnected) => {
                        if listener_connected {
                            listener_connected = false;
                            reconcile_tick = build_reconcile_tick(listener_connected);
                            warn!(
                                reconcile_interval_secs = NOTIFY_DEGRADED_RECONCILE_INTERVAL_SECS,
                                "stream config notify listener unavailable; using degraded periodic fallback"
                            );
                        }
                    }
                    None => {
                        if listener_connected {
                            listener_connected = false;
                            reconcile_tick = build_reconcile_tick(listener_connected);
                        }
                        warn!("stream config notify listener stopped; continuing with degraded periodic reconcile");
                    }
                }
                if !should_reconcile {
                    continue;
                }
                if let Err(error) = reconcile(&repo, raw_repo.as_ref(), &stream, &mut workers).await {
                    common::log_error!(warn, error, "stream supervisor notify-driven reconcile failed");
                }
            }
            _ = purge_tick.tick(), if purge_enabled => {
                match repo.purge_old_tick_events(DEFAULT_RETENTION_SECONDS).await {
                    Ok(deleted) => {
                        if deleted > 0 {
                            info!(deleted, retention_seconds = DEFAULT_RETENTION_SECONDS, "purged old quote/trade source feed rows");
                        }
                    }
                    Err(error) => common::log_error!(warn, error, "failed purging old quote/trade source feed rows"),
                }
            }
        }
    }
}

fn build_reconcile_tick(listener_connected: bool) -> tokio::time::Interval {
    let reconcile_secs = if listener_connected {
        NOTIFY_CONNECTED_RECONCILE_INTERVAL_SECS
    } else {
        NOTIFY_DEGRADED_RECONCILE_INTERVAL_SECS
    };
    interval(Duration::from_secs(reconcile_secs))
}

async fn reconcile(
    repo: &PostgresRepository,
    raw_repo: Option<&PostgresRawRepository>,
    stream: &RedisStreamPublisher,
    workers: &mut HashMap<String, WorkerHandle>,
) -> Result<()> {
    let effective = repo.list_effective_stream_configs().await?;
    let mut desired_ids: Vec<String> = Vec::new();

    for cfg in effective {
        if let Some(reason) = skipped_test_stream_reason(&cfg) {
            info!(
                stream_config_id = %cfg.stream_config_id,
                source_id = %cfg.source_id,
                stream_name = %cfg.stream_name,
                connector_mode = %cfg.connector_mode,
                reason,
                "skipping deprecated test stream config",
            );
            continue;
        }

        let targets = repo
            .list_stream_tenant_targets(&cfg.stream_config_id, &cfg.operating_mode_profile)
            .await?
            .into_iter()
            .map(|target| target.tenant_id)
            .collect::<Vec<_>>();
        if targets.is_empty() {
            continue;
        }

        let runtime_cfg = to_runtime_config(cfg, targets);
        log_test_mode_stream_selection(&runtime_cfg);
        let config_hash = hash_runtime_config(&runtime_cfg);
        desired_ids.push(runtime_cfg.stream_config_id.clone());

        if let Some(existing) = workers.get(&runtime_cfg.stream_config_id) {
            if existing.config_hash == config_hash {
                continue;
            }
        }

        if let Some(previous) = workers.remove(&runtime_cfg.stream_config_id) {
            stop_worker(previous).await;
        }

        let (stop_tx, stop_rx) = watch::channel(false);
        let stream_config_id = runtime_cfg.stream_config_id.clone();
        let source_id = runtime_cfg.source_id.clone();
        let repo_clone = repo.clone();
        let raw_repo_clone = raw_repo.cloned();
        let stream_clone = stream.clone();
        let join_handle = tokio::spawn(async move {
            run_stream_worker(
                runtime_cfg,
                repo_clone,
                raw_repo_clone,
                stream_clone,
                stop_rx,
            )
            .await;
        });

        info!(stream_config_id = %stream_config_id, source_id = %source_id, "stream worker started by reconcile");
        workers.insert(
            stream_config_id,
            WorkerHandle {
                config_hash,
                stop_tx,
                join_handle,
            },
        );
    }

    let stale_ids = workers
        .keys()
        .filter(|stream_config_id| !desired_ids.iter().any(|id| id == *stream_config_id))
        .cloned()
        .collect::<Vec<_>>();

    for stream_config_id in stale_ids {
        if let Some(handle) = workers.remove(&stream_config_id) {
            info!(stream_config_id = %stream_config_id, "stream worker stopped by reconcile");
            stop_worker(handle).await;
        }
    }

    Ok(())
}

async fn stop_worker(handle: WorkerHandle) {
    let _ = handle.stop_tx.send(true);
    if let Err(error) = handle.join_handle.await {
        common::log_error!(warn, error, "stream worker join failed");
    }
}

fn skipped_test_stream_reason(cfg: &EffectiveStreamConfig) -> Option<&'static str> {
    if cfg.operating_mode_profile != "test" {
        return None;
    }

    for key in ["ws_endpoint", "rpc_url", "http_url", "endpoint"] {
        let Some(endpoint) = cfg
            .connection_config
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::trim)
        else {
            continue;
        };
        if endpoint.is_empty() {
            continue;
        }
        if endpoint.contains('{') && endpoint.contains('}') {
            return Some("unresolved endpoint placeholder");
        }

        let Ok(parsed) = Url::parse(endpoint) else {
            continue;
        };
        let path = parsed.path();
        if path.contains("/api/simulation/mock/") {
            let mut has_tenant_id = false;
            let mut has_stream_name = false;
            for (key, value) in parsed.query_pairs() {
                match key.as_ref() {
                    "tenant_id" if !value.trim().is_empty() => has_tenant_id = true,
                    "stream_name" if !value.trim().is_empty() => has_stream_name = true,
                    _ => {}
                }
            }
            if !has_tenant_id || !has_stream_name {
                return Some("deprecated simulation replay endpoint");
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::skipped_test_stream_reason;
    use state_manager::EffectiveStreamConfig;

    fn config_for(endpoint_key: &str, endpoint: &str) -> EffectiveStreamConfig {
        EffectiveStreamConfig {
            stream_config_id: "stream-1".to_string(),
            source_id: "source-1".to_string(),
            source_type: "cex".to_string(),
            source_name: "source".to_string(),
            connection_config: serde_json::json!({ endpoint_key: endpoint }),
            connector_mode: "websocket".to_string(),
            operating_mode_profile: "test".to_string(),
            stream_name: "ticker-usdc-usd".to_string(),
            subscription_key: None,
            event_type: "quote".to_string(),
            parser_name: "parser".to_string(),
            market_key: Some("USDC/USD".to_string()),
            asset_pair: Some("USDC/USD".to_string()),
            filter_config: serde_json::json!({}),
            auth_secret_ref: None,
            auth_config: serde_json::json!({}),
            payload_ts_path: None,
            payload_ts_unit: "ms".to_string(),
            poll_interval_ms: None,
        }
    }

    #[test]
    fn skips_bare_simulation_mock_routes() {
        let cfg = config_for(
            "rpc_url",
            "ws://workbench-services:8010/api/simulation/mock/rpc/chainlink-eth-mainnet",
        );
        assert_eq!(
            skipped_test_stream_reason(&cfg),
            Some("deprecated simulation replay endpoint")
        );
    }

    #[test]
    fn keeps_run_scoped_simulation_routes() {
        let cfg = config_for(
            "rpc_url",
            "ws://workbench-services:8010/api/simulation/mock/rpc/chainlink-eth-mainnet?tenant_id=raksha-demo&stream_name=usdc-usd-feed&simulation_run_id=run_123",
        );
        assert_eq!(skipped_test_stream_reason(&cfg), None);
    }

    #[test]
    fn skips_test_streams_with_unresolved_placeholders() {
        let cfg = config_for(
            "rpc_url",
            "{simlab_mock_rpc_base_url}/rpc/uniswap-v2-eth-mainnet",
        );
        assert_eq!(
            skipped_test_stream_reason(&cfg),
            Some("unresolved endpoint placeholder")
        );
    }
}

fn to_runtime_config(
    cfg: EffectiveStreamConfig,
    tenant_targets: Vec<String>,
) -> RuntimeStreamConfig {
    RuntimeStreamConfig {
        stream_config_id: cfg.stream_config_id,
        source_id: cfg.source_id,
        source_type: cfg.source_type,
        source_name: cfg.source_name,
        connection_config: cfg.connection_config,
        operating_mode_profile: cfg.operating_mode_profile,
        auth_secret_ref: cfg.auth_secret_ref,
        auth_config: cfg.auth_config,
        connector_mode: cfg.connector_mode,
        stream_name: cfg.stream_name,
        subscription_key: cfg.subscription_key,
        event_type: cfg.event_type,
        parser_name: cfg.parser_name,
        market_key: cfg.market_key,
        asset_pair: cfg.asset_pair,
        filter_config: cfg.filter_config,
        payload_ts_path: cfg.payload_ts_path,
        payload_ts_unit: cfg.payload_ts_unit,
        poll_interval_ms: cfg
            .poll_interval_ms
            .and_then(|value| u64::try_from(value).ok()),
        tenant_targets,
    }
}

fn hash_runtime_config(cfg: &RuntimeStreamConfig) -> String {
    let serializable = json!({
        "stream_config_id": cfg.stream_config_id,
        "source_id": cfg.source_id,
        "source_type": cfg.source_type,
        "source_name": cfg.source_name,
        "connection_config": cfg.connection_config,
        "operating_mode_profile": cfg.operating_mode_profile,
        "auth_secret_ref": cfg.auth_secret_ref,
        "auth_config": cfg.auth_config,
        "connector_mode": cfg.connector_mode,
        "stream_name": cfg.stream_name,
        "subscription_key": cfg.subscription_key,
        "event_type": cfg.event_type,
        "parser_name": cfg.parser_name,
        "market_key": cfg.market_key,
        "asset_pair": cfg.asset_pair,
        "filter_config": cfg.filter_config,
        "payload_ts_path": cfg.payload_ts_path,
        "payload_ts_unit": cfg.payload_ts_unit,
        "poll_interval_ms": cfg.poll_interval_ms,
        "tenant_targets": cfg.tenant_targets,
    });
    let mut hasher = Sha256::new();
    hasher.update(serializable.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

fn log_test_mode_stream_selection(cfg: &RuntimeStreamConfig) {
    if cfg.operating_mode_profile != "test" {
        return;
    }

    let endpoint = cfg
        .connection_config
        .as_object()
        .and_then(|config| {
            ["ws_endpoint", "rpc_url", "ws_url", "endpoint", "http_url"]
                .iter()
                .find_map(|key| config.get(*key).and_then(|value| value.as_str()))
        })
        .map(str::trim)
        .unwrap_or("");

    let parsed = Url::parse(endpoint).ok();
    let endpoint_path = parsed
        .as_ref()
        .map(|url| url.path().to_string())
        .unwrap_or_else(|| endpoint.to_string());
    let endpoint_host = parsed.as_ref().and_then(|url| url.host_str());
    let is_mock_endpoint = parsed
        .as_ref()
        .map(|url| url.path().contains("/api/simulation/mock/"))
        .unwrap_or_else(|| endpoint.contains("/api/simulation/mock/"));
    let mut endpoint_tenant_id: Option<String> = None;
    let mut endpoint_stream_name: Option<String> = None;
    let mut endpoint_simulation_run_id: Option<String> = None;
    if let Some(url) = parsed.as_ref() {
        for (key, value) in url.query_pairs() {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key.as_ref() {
                "tenant_id" => endpoint_tenant_id = Some(value.to_string()),
                "stream_name" => endpoint_stream_name = Some(value.to_string()),
                "simulation_run_id" => endpoint_simulation_run_id = Some(value.to_string()),
                _ => {}
            }
        }
    }

    info!(
        stream_config_id = %cfg.stream_config_id,
        source_id = %cfg.source_id,
        stream_name = %cfg.stream_name,
        connector_mode = %cfg.connector_mode,
        tenant_targets = ?cfg.tenant_targets,
        endpoint_host,
        endpoint_path = %endpoint_path,
        is_mock_endpoint,
        endpoint_tenant_id = endpoint_tenant_id.as_deref(),
        endpoint_stream_name = endpoint_stream_name.as_deref(),
        endpoint_simulation_run_id = endpoint_simulation_run_id.as_deref(),
        "selected test-mode stream config",
    );

    if !is_mock_endpoint {
        warn!(
            stream_config_id = %cfg.stream_config_id,
            source_id = %cfg.source_id,
            stream_name = %cfg.stream_name,
            connector_mode = %cfg.connector_mode,
            endpoint_host,
            endpoint_path = %endpoint_path,
            "selected test-mode stream config does not point to a mock endpoint",
        );
    } else if endpoint_tenant_id.is_none()
        || endpoint_stream_name.is_none()
        || endpoint_simulation_run_id.is_none()
    {
        warn!(
            stream_config_id = %cfg.stream_config_id,
            source_id = %cfg.source_id,
            stream_name = %cfg.stream_name,
            connector_mode = %cfg.connector_mode,
            endpoint_host,
            endpoint_path = %endpoint_path,
            endpoint_tenant_id = endpoint_tenant_id.as_deref(),
            endpoint_stream_name = endpoint_stream_name.as_deref(),
            endpoint_simulation_run_id = endpoint_simulation_run_id.as_deref(),
            "selected test-mode mock endpoint is missing expected query parameters",
        );
    }
}

fn spawn_config_notify_listener(database_url: String) -> mpsc::Receiver<NotifySignal> {
    let (tx, rx) = mpsc::channel(32);
    tokio::spawn(async move {
        loop {
            let connector = match make_postgres_tls_connector() {
                Ok(connector) => connector,
                Err(error) => {
                    common::log_error!(
                        warn,
                        error,
                        "failed to build postgres TLS connector for stream config LISTEN"
                    );
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };

            let (client, mut connection) = match tokio_postgres::connect(&database_url, connector)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    common::log_error!(warn, error, "failed to connect for stream config LISTEN");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };

            if let Err(error) = client
                .batch_execute("LISTEN source_stream_config_changed")
                .await
            {
                common::log_error!(
                    warn,
                    error,
                    "failed to execute LISTEN source_stream_config_changed"
                );
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }

            let _ = tx.send(NotifySignal::Connected).await;
            info!("stream config listener connected (LISTEN source_stream_config_changed)");

            loop {
                let message = poll_fn(|cx| connection.poll_message(cx)).await;
                match message {
                    Some(Ok(AsyncMessage::Notification(notification))) => {
                        if notification.channel() == "source_stream_config_changed"
                            && tx.send(NotifySignal::Changed).await.is_err()
                        {
                            return;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        common::log_error!(
                            warn,
                            error,
                            "stream config listener connection error; reconnecting"
                        );
                        let _ = tx.send(NotifySignal::Disconnected).await;
                        break;
                    }
                    None => {
                        let _ = tx.send(NotifySignal::Disconnected).await;
                        warn!("stream config listener connection closed; reconnecting");
                        break;
                    }
                }
            }
        }
    });
    rx
}

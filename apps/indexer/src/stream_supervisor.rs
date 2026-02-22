use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use futures_util::future::poll_fn;
use serde_json::json;
use sha2::{Digest, Sha256};
use state_manager::{EffectiveStreamConfig, PostgresRepository, RedisStreamPublisher};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::interval,
};
use tokio_postgres::{AsyncMessage, NoTls};
use tracing::{info, warn};

use crate::stream_worker::{run_stream_worker, RuntimeStreamConfig};

const DEFAULT_RECONCILE_INTERVAL_SECS: u64 = 30;
const DEFAULT_PURGE_INTERVAL_SECS: u64 = 15;
const DEFAULT_RETENTION_SECONDS: i64 = 300;

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
    let mut workers: HashMap<String, WorkerHandle> = HashMap::new();
    let mut notify_rx = spawn_config_notify_listener(database_url);
    let mut reconcile_tick = interval(Duration::from_secs(DEFAULT_RECONCILE_INTERVAL_SECS));
    let mut purge_tick = interval(Duration::from_secs(DEFAULT_PURGE_INTERVAL_SECS));

    reconcile(&repo, &stream, &mut workers).await?;

    loop {
        tokio::select! {
            _ = reconcile_tick.tick() => {
                if let Err(error) = reconcile(&repo, &stream, &mut workers).await {
                    warn!(error = ?error, "stream supervisor periodic reconcile failed");
                }
            }
            signal = notify_rx.recv() => {
                if signal.is_none() {
                    warn!("stream config notify listener stopped; continuing with periodic reconcile only");
                }
                if let Err(error) = reconcile(&repo, &stream, &mut workers).await {
                    warn!(error = ?error, "stream supervisor notify-driven reconcile failed");
                }
            }
            _ = purge_tick.tick(), if purge_enabled => {
                match repo.purge_old_tick_events(DEFAULT_RETENTION_SECONDS).await {
                    Ok(deleted) => {
                        if deleted > 0 {
                            info!(deleted, retention_seconds = DEFAULT_RETENTION_SECONDS, "purged old quote/trade source feed rows");
                        }
                    }
                    Err(error) => warn!(error = ?error, "failed purging old quote/trade source feed rows"),
                }
            }
        }
    }
}

async fn reconcile(
    repo: &PostgresRepository,
    stream: &RedisStreamPublisher,
    workers: &mut HashMap<String, WorkerHandle>,
) -> Result<()> {
    let effective = repo.list_effective_stream_configs().await?;
    let mut desired_ids: Vec<String> = Vec::new();

    for cfg in effective {
        let targets = repo
            .list_stream_tenant_targets(&cfg.stream_config_id)
            .await?
            .into_iter()
            .map(|target| target.tenant_id)
            .collect::<Vec<_>>();
        if targets.is_empty() {
            continue;
        }

        let runtime_cfg = to_runtime_config(cfg, targets);
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
        let stream_clone = stream.clone();
        let join_handle = tokio::spawn(async move {
            run_stream_worker(runtime_cfg, repo_clone, stream_clone, stop_rx).await;
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
        warn!(error = ?error, "stream worker join failed");
    }
}

fn to_runtime_config(cfg: EffectiveStreamConfig, tenant_targets: Vec<String>) -> RuntimeStreamConfig {
    RuntimeStreamConfig {
        stream_config_id: cfg.stream_config_id,
        source_id: cfg.source_id,
        source_type: cfg.source_type,
        source_name: cfg.source_name,
        connection_config: cfg.connection_config,
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
        "tenant_targets": cfg.tenant_targets,
    });
    let mut hasher = Sha256::new();
    hasher.update(serializable.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

fn spawn_config_notify_listener(database_url: String) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel(32);
    tokio::spawn(async move {
        loop {
            let (client, mut connection) = match tokio_postgres::connect(&database_url, NoTls).await {
                Ok(value) => value,
                Err(error) => {
                    warn!(error = ?error, "failed to connect for stream config LISTEN");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };

            if let Err(error) = client.batch_execute("LISTEN source_stream_config_changed").await {
                warn!(error = ?error, "failed to execute LISTEN source_stream_config_changed");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }

            info!("stream config listener connected (LISTEN source_stream_config_changed)");

            loop {
                let message = poll_fn(|cx| connection.poll_message(cx)).await;
                match message {
                    Some(Ok(AsyncMessage::Notification(notification))) => {
                        if notification.channel() == "source_stream_config_changed" {
                            if tx.send(()).await.is_err() {
                                return;
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        warn!(error = ?error, "stream config listener connection error; reconnecting");
                        break;
                    }
                    None => {
                        warn!("stream config listener connection closed; reconnecting");
                        break;
                    }
                }
            }
        }
    });
    rx
}

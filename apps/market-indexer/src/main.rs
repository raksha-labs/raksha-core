use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use common::ShutdownSignal;
use dotenv::dotenv;
use event_schema::MarketQuoteEvent;
use market_connectors::{build_connector, MarketSourceConfig, ParsedQuote};
use state_manager::{MarketPairConfigRow, MarketSourceConfigRow, PostgresRepository, RedisStreamPublisher};
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Debug, Clone)]
struct PairTarget {
    market_key: String,
    peg_target: f64,
}

#[derive(Debug, Clone)]
struct QuoteEnvelope {
    source: MarketSourceConfigRow,
    quote: ParsedQuote,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .compact()
        .init();

    if !env_bool("MARKET_DPEG_ENABLED", true) {
        info!("market-indexer disabled by MARKET_DPEG_ENABLED flag");
        return Ok(());
    }

    let Some(stream) = init_stream_publisher().await else {
        warn!("market-indexer requires REDIS_URL");
        return Ok(());
    };
    let Some(repo) = init_repository().await else {
        warn!("market-indexer requires DATABASE_URL");
        return Ok(());
    };

    let shutdown = ShutdownSignal::install();

    let sources = repo.load_enabled_market_sources().await?;
    let pairs = repo.load_enabled_market_pairs().await?;
    if sources.is_empty() {
        warn!("no enabled tenant_market_sources found; market-indexer exiting");
        return Ok(());
    }
    if pairs.is_empty() {
        warn!("no enabled tenant_market_pairs found; market-indexer exiting");
        return Ok(());
    }

    let pair_lookup = build_pair_lookup(&pairs);
    let source_symbols = build_source_symbols(&pairs);

    let (tx, mut rx) = mpsc::channel::<QuoteEnvelope>(2000);

    for source in sources {
        let symbols = source_symbols
            .get(&(source.tenant_id.clone(), source.source_id.clone()))
            .cloned()
            .unwrap_or_default();

        if symbols.is_empty() {
            warn!(
                tenant_id = %source.tenant_id,
                source_id = %source.source_id,
                "source has no enabled pair symbols; skipping connector"
            );
            continue;
        }

        let tx_clone = tx.clone();
        let repo_clone = repo.clone();
        let shutdown_clone = shutdown.clone();
        tokio::spawn(async move {
            run_source_connector(source, symbols, tx_clone, repo_clone, shutdown_clone).await;
        });
    }
    drop(tx);

    info!("market-indexer started");
    while !shutdown.is_shutdown_requested() {
        let Some(envelope) = rx.recv().await else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };

        let source_symbol_key = normalize_symbol(&envelope.quote.source_symbol);
        let key = (
            envelope.source.tenant_id.clone(),
            envelope.source.source_id.clone(),
            source_symbol_key,
        );
        let Some(targets) = pair_lookup.get(&key) else {
            warn!(
                tenant_id = %envelope.source.tenant_id,
                source_id = %envelope.source.source_id,
                source_symbol = %envelope.quote.source_symbol,
                "received quote without configured market mapping"
            );
            continue;
        };

        for target in targets {
            let mut metadata = serde_json::Map::new();
            metadata.insert(
                "source_metadata".to_string(),
                envelope.source.metadata.clone(),
            );
            metadata.insert("raw_quote".to_string(), envelope.quote.metadata.clone());

            let event = MarketQuoteEvent {
                quote_id: Uuid::new_v4(),
                tenant_id: envelope.source.tenant_id.clone(),
                source_id: envelope.source.source_id.clone(),
                source_kind: envelope.source.source_kind.clone(),
                source_name: envelope.source.source_name.clone(),
                market_key: target.market_key.clone(),
                source_symbol: envelope.quote.source_symbol.clone(),
                price: envelope.quote.price,
                peg_target: target.peg_target,
                observed_at: envelope.quote.observed_at,
                metadata: metadata
                    .into_iter()
                    .map(|(k, v)| (k, v))
                    .collect::<HashMap<_, _>>(),
            };

            if let Err(err) = repo.save_market_quote(&event).await {
                warn!(error = ?err, "failed to persist market quote");
            }
            if let Err(err) = stream.publish_market_quote(&event).await {
                warn!(error = ?err, "failed to publish market quote stream event");
            }

            if let Err(err) = repo
                .upsert_connector_health(
                    &envelope.source.tenant_id,
                    &envelope.source.source_id,
                    true,
                    Some(Utc::now()),
                    None,
                )
                .await
            {
                warn!(error = ?err, "failed to update connector health");
            }
        }
    }

    info!("market-indexer stopping");
    Ok(())
}

async fn run_source_connector(
    source: MarketSourceConfigRow,
    symbols: Vec<String>,
    tx: mpsc::Sender<QuoteEnvelope>,
    repo: PostgresRepository,
    shutdown: ShutdownSignal,
) {
    let mut backoff_secs = 1_u64;

    while !shutdown.is_shutdown_requested() {
        let source_cfg = MarketSourceConfig {
            tenant_id: source.tenant_id.clone(),
            source_id: source.source_id.clone(),
            source_kind: source.source_kind.clone(),
            source_name: source.source_name.clone(),
            ws_endpoint: source.ws_endpoint.clone(),
            metadata: source.metadata.clone(),
        };

        match build_connector(source_cfg.clone()) {
            Ok(mut connector) => {
                let connect_result = match connector.connect().await {
                    Ok(_) => connector.subscribe(&symbols).await,
                    Err(err) => Err(err),
                };

                if let Err(err) = connect_result {
                    warn!(
                        tenant_id = %source.tenant_id,
                        source_id = %source.source_id,
                        error = ?err,
                        "connector connect/subscribe failed"
                    );
                    let _ = repo
                        .upsert_connector_health(
                            &source.tenant_id,
                            &source.source_id,
                            false,
                            None,
                            Some(&err.to_string()),
                        )
                        .await;
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                    continue;
                }

                backoff_secs = 1;
                loop {
                    if shutdown.is_shutdown_requested() {
                        return;
                    }

                    match connector.next_quote().await {
                        Ok(quote) => {
                            if tx
                                .send(QuoteEnvelope {
                                    source: source.clone(),
                                    quote,
                                })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(err) => {
                            warn!(
                                tenant_id = %source.tenant_id,
                                source_id = %source.source_id,
                                error = ?err,
                                "connector quote read failed; reconnecting"
                            );
                            let _ = repo
                                .upsert_connector_health(
                                    &source.tenant_id,
                                    &source.source_id,
                                    false,
                                    None,
                                    Some(&err.to_string()),
                                )
                                .await;
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                warn!(
                    tenant_id = %source.tenant_id,
                    source_id = %source.source_id,
                    error = ?err,
                    "unsupported source connector"
                );
                let _ = repo
                    .upsert_connector_health(
                        &source.tenant_id,
                        &source.source_id,
                        false,
                        None,
                        Some(&err.to_string()),
                    )
                    .await;
                return;
            }
        }

        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

fn build_pair_lookup(
    pairs: &[MarketPairConfigRow],
) -> HashMap<(String, String, String), Vec<PairTarget>> {
    let mut lookup: HashMap<(String, String, String), Vec<PairTarget>> = HashMap::new();
    for row in pairs {
        lookup
            .entry((
                row.tenant_id.clone(),
                row.source_id.clone(),
                normalize_symbol(&row.source_symbol),
            ))
            .or_default()
            .push(PairTarget {
                market_key: row.market_key.clone(),
                peg_target: row.peg_target,
            });
    }

    lookup
}

fn build_source_symbols(
    pairs: &[MarketPairConfigRow],
) -> HashMap<(String, String), Vec<String>> {
    let mut by_source: HashMap<(String, String), Vec<String>> = HashMap::new();

    for row in pairs {
        let key = (row.tenant_id.clone(), row.source_id.clone());
        let entry = by_source.entry(key).or_default();
        if !entry.iter().any(|symbol| symbol == &row.source_symbol) {
            entry.push(row.source_symbol.clone());
        }
    }

    by_source
}

fn normalize_symbol(raw: &str) -> String {
    raw.trim().to_ascii_uppercase()
}

async fn init_stream_publisher() -> Option<RedisStreamPublisher> {
    let Some(publisher_result) = RedisStreamPublisher::from_env() else {
        return None;
    };

    let publisher = match publisher_result {
        Ok(publisher) => publisher,
        Err(err) => {
            warn!(error = ?err, "invalid REDIS_URL; redis streams disabled");
            return None;
        }
    };

    if let Err(err) = publisher.healthcheck().await {
        warn!(error = ?err, "redis healthcheck failed");
        None
    } else {
        Some(publisher)
    }
}

async fn init_repository() -> Option<PostgresRepository> {
    let Some(database_url) = PostgresRepository::from_env() else {
        return None;
    };

    match PostgresRepository::from_database_url(&database_url).await {
        Ok(repo) => Some(repo),
        Err(err) => {
            warn!(error = ?err, "failed to connect to postgres");
            None
        }
    }
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(default)
}

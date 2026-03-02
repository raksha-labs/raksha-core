use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub chains: HashMap<String, ChainConfig>,
    pub reference_price: ReferencePriceConfig,
    pub alert_sinks: AlertSinksConfig,
    pub tenancy: TenancyConfig,
    pub state_manager: PersistenceConfig,
    pub trace_enrichment: TraceEnrichmentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainConfig {
    pub name: String,
    pub rpc_ws_url: String,
    pub rpc_http_url: String,
    pub enabled: bool,
    pub confirmation_depth: u64,
    pub protocols: Vec<ProtocolConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolConfig {
    pub name: String,
    pub enabled: bool,
    pub oracle_addresses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencePriceConfig {
    pub binance_enabled: bool,
    pub uniswap_enabled: bool,
    pub uniswap_pools: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertSinksConfig {
    pub webhook_url: Option<String>,
    pub slack_webhook_url: Option<String>,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub discord_webhook_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenancyConfig {
    pub enabled: bool,
    pub default_tenant_id: String,
    pub enforce_route_policies: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceConfig {
    pub postgres_url: String,
    pub max_connections: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEnrichmentConfig {
    pub enabled: bool,
    pub redis_url: String,
    pub timeout_secs: u64,
}

impl AppConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let eth_config = ChainConfig {
            name: "ethereum".to_string(),
            rpc_ws_url: env::var("ETH_RPC_WS_URL")
                .context("Missing ETH_RPC_WS_URL environment variable")?,
            rpc_http_url: env::var("ETH_RPC_HTTP_URL")
                .context("Missing ETH_RPC_HTTP_URL environment variable")?,
            enabled: env::var("ETH_ENABLED")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .unwrap_or(true),
            confirmation_depth: env::var("ETH_CONFIRMATION_DEPTH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
            protocols: Vec::new(), // Loaded from protocol_config.yaml
        };

        let base_config = ChainConfig {
            name: "base".to_string(),
            rpc_ws_url: env::var("BASE_RPC_WS_URL")
                .context("Missing BASE_RPC_WS_URL environment variable")?,
            rpc_http_url: env::var("BASE_RPC_HTTP_URL")
                .context("Missing BASE_RPC_HTTP_URL environment variable")?,
            enabled: env::var("BASE_ENABLED")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .unwrap_or(true),
            confirmation_depth: env::var("BASE_CONFIRMATION_DEPTH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(64), // L2 needs more confirmations
            protocols: Vec::new(),
        };

        let mut chains = HashMap::new();
        chains.insert("ethereum".to_string(), eth_config);
        chains.insert("base".to_string(), base_config);

        let reference_price = ReferencePriceConfig {
            binance_enabled: env::var("BINANCE_ENABLED")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .unwrap_or(true),
            uniswap_enabled: env::var("UNISWAP_ENABLED")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .unwrap_or(true),
            uniswap_pools: HashMap::new(), // Loaded from protocol_config.yaml
        };

        let alert_sinks = AlertSinksConfig {
            webhook_url: env::var("WEBHOOK_URL").ok(),
            slack_webhook_url: env::var("SLACK_WEBHOOK_URL").ok(),
            telegram_bot_token: env::var("TELEGRAM_BOT_TOKEN").ok(),
            telegram_chat_id: env::var("TELEGRAM_CHAT_ID").ok(),
            discord_webhook_url: env::var("DISCORD_WEBHOOK_URL").ok(),
        };

        let tenancy = TenancyConfig {
            enabled: env::var("TENANCY_ENABLED")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            default_tenant_id: env::var("DEFAULT_TENANT_ID")
                .unwrap_or_else(|_| "default".to_string()),
            enforce_route_policies: env::var("TENANT_ROUTE_POLICY_ENFORCED")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .unwrap_or(true),
        };

        let state_manager = PersistenceConfig {
            postgres_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgresql://localhost/raksha".to_string()),
            max_connections: env::var("DB_MAX_CONNECTIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
        };

        let trace_enrichment = TraceEnrichmentConfig {
            enabled: env::var("TRACE_ENRICHMENT_ENABLED")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            redis_url: env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://localhost:6379".to_string()),
            timeout_secs: env::var("TRACE_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
        };

        Ok(Self {
            chains,
            reference_price,
            alert_sinks,
            tenancy,
            state_manager,
            trace_enrichment,
        })
    }

    /// Load protocol configurations from YAML files
    pub fn load_protocol_configs(&mut self, rules_base_path: impl AsRef<Path>) -> Result<()> {
        let base_path = rules_base_path.as_ref();

        for (chain_name, chain_config) in self.chains.iter_mut() {
            let protocol_config_path = base_path
                .join("chains")
                .join(chain_name)
                .join("protocol_config.yaml");

            if !protocol_config_path.exists() {
                continue;
            }

            let content = std::fs::read_to_string(&protocol_config_path)
                .with_context(|| format!("Failed to read {}", protocol_config_path.display()))?;

            let config: ProtocolConfigFile = serde_yaml::from_str(&content)
                .with_context(|| format!("Failed to parse {}", protocol_config_path.display()))?;

            chain_config.protocols = config.protocols;

            // Load Uniswap pools if present
            if let Some(pools) = config.uniswap_pools {
                self.reference_price.uniswap_pools.extend(pools);
            }
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct ProtocolConfigFile {
    protocols: Vec<ProtocolConfig>,
    #[serde(default)]
    uniswap_pools: Option<HashMap<String, String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_loads_from_env() {
        // Set test environment variables
        std::env::set_var("ETH_RPC_WS_URL", "wss://test.example.com");
        std::env::set_var("ETH_RPC_HTTP_URL", "https://test.example.com");
        std::env::set_var("BASE_RPC_WS_URL", "wss://base-test.example.com");
        std::env::set_var("BASE_RPC_HTTP_URL", "https://base-test.example.com");

        let config = AppConfig::from_env();
        assert!(config.is_ok());
    }
}

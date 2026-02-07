use anyhow::{bail, Result};
use common_types::ProtocolCategory;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct EvmChainConfig {
    pub family: String,
    pub chain_slug: String,
    pub chain_id: u64,
    pub ws_url_env: String,
    #[serde(default)]
    pub ws_url_env_fallbacks: Vec<String>,
    pub lookback_blocks: Option<u64>,
    pub default_oracle_decimals: Option<u8>,
    pub protocol_category_default: Option<String>,
}

impl EvmChainConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.family.eq_ignore_ascii_case("evm") {
            bail!(
                "unsupported family '{}' for chain '{}'; expected 'evm'",
                self.family,
                self.chain_slug
            );
        }

        if self.chain_slug.is_empty() {
            bail!("chain_slug must not be empty");
        }

        let is_kebab = self
            .chain_slug
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
        if !is_kebab {
            bail!(
                "chain_slug '{}' must be lowercase kebab-case",
                self.chain_slug
            );
        }

        if self.ws_url_env.trim().is_empty() {
            bail!(
                "ws_url_env must be configured for chain '{}'",
                self.chain_slug
            );
        }
        if self
            .ws_url_env_fallbacks
            .iter()
            .any(|name| name.trim().is_empty())
        {
            bail!(
                "ws_url_env_fallbacks must not contain empty env names for chain '{}'",
                self.chain_slug
            );
        }

        Ok(())
    }

    pub fn ws_env_names(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(1 + self.ws_url_env_fallbacks.len());
        out.push(self.ws_url_env.clone());
        for name in &self.ws_url_env_fallbacks {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
        out
    }

    pub fn protocol_category_default(&self) -> ProtocolCategory {
        self.protocol_category_default
            .as_deref()
            .map(parse_protocol_category)
            .unwrap_or_else(|| default_protocol_category_for_chain(&self.chain_slug))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvmProtocolConfigFile {
    pub lookback_blocks: Option<u64>,
    pub default_oracle_decimals: Option<u8>,
    pub protocols: Vec<EvmProtocolConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvmProtocolConfig {
    pub protocol: String,
    pub enabled: bool,
    pub oracle_addresses: Vec<String>,
    pub oracle_decimals: Option<u8>,
    pub protocol_category: Option<String>,

    // Backward compatibility with old protocol-scoped ws env.
    #[serde(default)]
    pub ws_url_env: Option<String>,
}

pub fn parse_protocol_category(raw: &str) -> ProtocolCategory {
    match raw.trim().to_ascii_lowercase().as_str() {
        "lending" => ProtocolCategory::Lending,
        "perp" | "perp_dex" | "perpdex" | "perp-dex" => ProtocolCategory::PerpDex,
        _ => ProtocolCategory::Unknown,
    }
}

pub fn default_protocol_category_for_chain(chain_slug: &str) -> ProtocolCategory {
    match chain_slug {
        "ethereum" => ProtocolCategory::Lending,
        "base" => ProtocolCategory::PerpDex,
        _ => ProtocolCategory::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_evm_chain_config() {
        let cfg = EvmChainConfig {
            family: "evm".to_string(),
            chain_slug: "ethereum".to_string(),
            chain_id: 1,
            ws_url_env: "ETH_WS_URL".to_string(),
            ws_url_env_fallbacks: vec!["ETH_WS_URL_BACKUP".to_string()],
            lookback_blocks: Some(300),
            default_oracle_decimals: Some(8),
            protocol_category_default: Some("lending".to_string()),
        };

        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.protocol_category_default(), ProtocolCategory::Lending);
    }

    #[test]
    fn rejects_invalid_chain_slug() {
        let cfg = EvmChainConfig {
            family: "evm".to_string(),
            chain_slug: "Ethereum Mainnet".to_string(),
            chain_id: 1,
            ws_url_env: "ETH_WS_URL".to_string(),
            ws_url_env_fallbacks: vec![],
            lookback_blocks: None,
            default_oracle_decimals: None,
            protocol_category_default: None,
        };

        assert!(cfg.validate().is_err());
    }

    #[test]
    fn ws_env_names_keeps_primary_then_unique_fallbacks() {
        let cfg = EvmChainConfig {
            family: "evm".to_string(),
            chain_slug: "ethereum".to_string(),
            chain_id: 1,
            ws_url_env: "ETH_WS_URL".to_string(),
            ws_url_env_fallbacks: vec![
                "ETH_WS_URL_BACKUP".to_string(),
                "ETH_WS_URL".to_string(),
                "ETH_WS_URL_ALT".to_string(),
            ],
            lookback_blocks: None,
            default_oracle_decimals: None,
            protocol_category_default: None,
        };

        assert_eq!(
            cfg.ws_env_names(),
            vec![
                "ETH_WS_URL".to_string(),
                "ETH_WS_URL_BACKUP".to_string(),
                "ETH_WS_URL_ALT".to_string()
            ]
        );
    }
}

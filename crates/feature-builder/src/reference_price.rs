use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use ethers::{
    providers::{Http, Provider},
    types::Address,
};
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, warn};

#[async_trait]
pub trait ReferencePriceProvider: Send + Sync {
    async fn get_price(&self, pair: &str) -> Result<f64>;
}

/// Static provider for testing
pub struct StaticReferencePriceProvider;

#[async_trait]
impl ReferencePriceProvider for StaticReferencePriceProvider {
    async fn get_price(&self, pair: &str) -> Result<f64> {
        let price = match pair {
            "ETH/USD" => 3200.0,
            "BTC/USD" => 95_000.0,
            "USDC/USD" => 1.0,
            _ => 0.0,
        };
        Ok(price)
    }
}

/// Binance API response for ticker price
#[derive(Debug, Deserialize)]
struct BinanceTickerResponse {
    #[allow(dead_code)]
    symbol: String,
    price: String,
}

/// Live reference price provider using multiple sources
pub struct LiveReferencePriceProvider {
    http_client: reqwest::Client,
    eth_provider: Option<Arc<Provider<Http>>>,
    uniswap_pools: HashMap<String, Address>,
    binance_enabled: bool,
    uniswap_enabled: bool,
}

impl LiveReferencePriceProvider {
    pub fn new(
        http_rpc_url: Option<String>,
        uniswap_pools: HashMap<String, Address>,
        binance_enabled: bool,
        uniswap_enabled: bool,
    ) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("Failed to create HTTP client")?;

        let eth_provider = if let Some(url) = http_rpc_url {
            Some(Arc::new(
                Provider::<Http>::try_from(&url)
                    .context("Failed to create Ethereum HTTP provider")?,
            ))
        } else {
            None
        };

        Ok(Self {
            http_client,
            eth_provider,
            uniswap_pools,
            binance_enabled,
            uniswap_enabled,
        })
    }

    /// Fetch price from Binance API
    async fn fetch_binance_price(&self, pair: &str) -> Result<f64> {
        let binance_symbol = match pair {
            "ETH/USD" => "ETHUSDT",
            "BTC/USD" => "BTCUSDT",
            "USDC/USD" => return Ok(1.0), // Stablecoin
            _ => return Err(anyhow!("Unsupported pair for Binance: {}", pair)),
        };

        let url = format!(
            "https://api.binance.com/api/v3/ticker/price?symbol={}",
            binance_symbol
        );

        let response: BinanceTickerResponse = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch from Binance")?
            .json()
            .await
            .context("Failed to parse Binance response")?;

        let price = response
            .price
            .parse::<f64>()
            .context("Failed to parse Binance price")?;

        debug!("Binance price for {}: ${:.2}", pair, price);
        Ok(price)
    }

    /// Fetch Uniswap V3 TWAP (Time-Weighted Average Price)
    /// This is a simplified version - production would use observe() with proper time windows
    async fn fetch_uniswap_twap(&self, pair: &str) -> Result<f64> {
        let _provider = self
            .eth_provider
            .as_ref()
            .ok_or_else(|| anyhow!("No Ethereum provider configured for Uniswap"))?;

        let pool_address = self
            .uniswap_pools
            .get(pair)
            .ok_or_else(|| anyhow!("No Uniswap pool configured for pair: {}", pair))?;

        // For now, return a simplified implementation
        // In production, this would:
        // 1. Call pool.observe([600, 0]) to get tick cumulatives
        // 2. Calculate time-weighted average tick
        // 3. Convert tick to price using tick math

        debug!(
            "Uniswap TWAP fetch for {} from pool {:?}",
            pair, pool_address
        );

        // Placeholder - would need actual Uniswap V3 pool contract call
        Ok(3200.0)
    }

    /// Get weighted average from multiple sources
    async fn get_averaged_price(&self, pair: &str) -> Result<f64> {
        let mut prices = Vec::new();

        if self.binance_enabled {
            match self.fetch_binance_price(pair).await {
                Ok(price) if price > 0.0 => prices.push(price),
                Ok(_) => warn!("Binance returned invalid price for {}", pair),
                Err(e) => warn!("Failed to fetch Binance price for {}: {:?}", pair, e),
            }
        }

        if self.uniswap_enabled && self.eth_provider.is_some() {
            match self.fetch_uniswap_twap(pair).await {
                Ok(price) if price > 0.0 => prices.push(price),
                Ok(_) => warn!("Uniswap returned invalid price for {}", pair),
                Err(e) => warn!("Failed to fetch Uniswap TWAP for {}: {:?}", pair, e),
            }
        }

        if prices.is_empty() {
            return Err(anyhow!("No valid price sources available for {}", pair));
        }

        // Simple average (could be weighted)
        let avg = prices.iter().sum::<f64>() / prices.len() as f64;

        debug!(
            "Reference price for {} from {} sources: ${:.2}",
            pair,
            prices.len(),
            avg
        );

        Ok(avg)
    }
}

#[async_trait]
impl ReferencePriceProvider for LiveReferencePriceProvider {
    async fn get_price(&self, pair: &str) -> Result<f64> {
        self.get_averaged_price(pair).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_static_provider() {
        let provider = StaticReferencePriceProvider;
        // Can't easily test async in sync test without tokio runtime
        // Just verify it compiles
    }

    #[test]
    fn test_live_provider_creation() {
        let pools = HashMap::new();
        let provider = LiveReferencePriceProvider::new(None, pools, true, false);
        assert!(provider.is_ok());
    }
}

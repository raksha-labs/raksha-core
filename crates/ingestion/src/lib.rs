pub mod adapter;
pub mod config;
pub mod connector;
pub mod decode_chainlink;
pub mod decode_flashloan;
pub mod mock;
pub mod normalize;
pub mod protocol_map;

pub use adapter::{EvmChainAdapter, RpcProviderStatus};
pub use connector::{CexWebsocketConnector, DataSourceConnector, EvmChainConnector};
pub use config::{
    default_protocol_category_for_chain, parse_protocol_category, EvmChainConfig,
    EvmProtocolConfig, EvmProtocolConfigFile, FlashLoanSourceConfig,
};
pub use mock::{EvmMockAdapter, MockProtocol};
pub use protocol_map::{
    build_oracle_protocol_map, parse_oracle_addresses, parse_oracle_addresses_csv,
    OracleProtocolMap, ProtocolBinding,
};

use anyhow::Result;
use ethers::types::Address;

pub const DEFAULT_LOOKBACK_BLOCKS: u64 = 300;
pub const DEFAULT_ORACLE_DECIMALS: u8 = 8;
pub const DEFAULT_FILTER_CHUNK_SIZE: usize = 250;

pub fn default_eth_oracles() -> Result<Vec<Address>> {
    parse_oracle_addresses_csv(
        "0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419,0xF4030086522a5bEEa4988F8cA5B36dbC97BeE88c,0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6",
    )
}

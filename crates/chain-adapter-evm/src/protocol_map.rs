use std::{collections::HashMap, str::FromStr};

use anyhow::{Context, Result};
use common_types::ProtocolCategory;
use ethers::types::Address;

#[derive(Debug, Clone)]
pub struct ProtocolBinding {
    pub protocol: String,
    pub oracle_decimals: u8,
    pub protocol_category: ProtocolCategory,
}

pub type OracleProtocolMap = HashMap<Address, Vec<ProtocolBinding>>;

pub fn parse_oracle_addresses_csv(csv: &str) -> Result<Vec<Address>> {
    csv.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            Address::from_str(entry)
                .with_context(|| format!("invalid oracle address in csv: {entry}"))
        })
        .collect()
}

pub fn parse_oracle_addresses(values: &[String]) -> Result<Vec<Address>> {
    values
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            Address::from_str(entry)
                .with_context(|| format!("invalid oracle address in protocol config: {entry}"))
        })
        .collect()
}

pub fn build_oracle_protocol_map(
    entries: impl IntoIterator<Item = (Vec<Address>, ProtocolBinding)>,
) -> OracleProtocolMap {
    let mut map: OracleProtocolMap = HashMap::new();

    for (addresses, binding) in entries {
        for address in addresses {
            map.entry(address).or_default().push(binding.clone());
        }
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_merged_oracle_protocol_map() {
        let shared =
            Address::from_str("0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419").expect("valid address");
        let second =
            Address::from_str("0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6").expect("valid address");

        let map = build_oracle_protocol_map(vec![
            (
                vec![shared, second],
                ProtocolBinding {
                    protocol: "aave-v3".to_string(),
                    oracle_decimals: 8,
                    protocol_category: ProtocolCategory::Lending,
                },
            ),
            (
                vec![shared],
                ProtocolBinding {
                    protocol: "compound-v3".to_string(),
                    oracle_decimals: 8,
                    protocol_category: ProtocolCategory::Lending,
                },
            ),
        ]);

        assert_eq!(map.get(&shared).map(Vec::len), Some(2));
        assert_eq!(map.get(&second).map(Vec::len), Some(1));
    }
}

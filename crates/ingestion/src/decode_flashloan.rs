use ethers::{
    types::{Address, Log, H256, U256},
    utils::keccak256,
};

pub const AAVE_FLASH_LOAN_EVENT: &str =
    "FlashLoan(address,address,address,uint256,uint8,uint256,uint16)";
pub const BALANCER_FLASH_LOAN_EVENT: &str = "FlashLoan(address,address,uint256,uint256)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFlashLoanLog {
    pub asset_address: Option<Address>,
    pub loan_amount_raw: U256,
    pub fee_raw: Option<U256>,
    pub borrower: Option<Address>,
    pub initiator: Option<Address>,
}

pub fn event_topic_from_signature(signature: &str) -> H256 {
    H256::from_slice(keccak256(signature.as_bytes()).as_slice())
}

pub fn decode_flash_loan_log(protocol: &str, log: &Log) -> Option<DecodedFlashLoanLog> {
    let protocol = protocol.to_ascii_lowercase();
    if protocol.contains("aave") {
        decode_aave_flash_loan(log)
    } else if protocol.contains("balancer") {
        decode_balancer_flash_loan(log)
    } else {
        None
    }
}

pub fn decode_aave_flash_loan(log: &Log) -> Option<DecodedFlashLoanLog> {
    if log.topics.len() < 4 {
        return None;
    }
    let data = log.data.as_ref();
    if data.len() < 64 {
        return None;
    }

    let loan_amount_raw = read_u256(data, 0)?;
    let fee_raw = if data.len() >= 128 {
        read_u256(data, 64)
    } else {
        read_u256(data, 32)
    };

    Some(DecodedFlashLoanLog {
        asset_address: Some(address_from_topic(&log.topics[3])),
        loan_amount_raw,
        fee_raw,
        borrower: Some(address_from_topic(&log.topics[1])),
        initiator: Some(address_from_topic(&log.topics[2])),
    })
}

pub fn decode_balancer_flash_loan(log: &Log) -> Option<DecodedFlashLoanLog> {
    if log.topics.len() < 3 {
        return None;
    }
    let data = log.data.as_ref();
    if data.len() < 64 {
        return None;
    }

    Some(DecodedFlashLoanLog {
        asset_address: Some(address_from_topic(&log.topics[2])),
        loan_amount_raw: read_u256(data, 0)?,
        fee_raw: read_u256(data, 32),
        borrower: Some(address_from_topic(&log.topics[1])),
        initiator: None,
    })
}

fn address_from_topic(topic: &H256) -> Address {
    Address::from_slice(&topic.as_bytes()[12..])
}

fn read_u256(data: &[u8], offset: usize) -> Option<U256> {
    let end = offset.saturating_add(32);
    if data.len() < end {
        return None;
    }
    Some(U256::from_big_endian(&data[offset..end]))
}

#[cfg(test)]
mod tests {
    use ethers::types::Log;

    use super::*;

    #[test]
    fn hashes_event_topic_from_signature() {
        assert_eq!(
            event_topic_from_signature(BALANCER_FLASH_LOAN_EVENT),
            H256::from_slice(
                keccak256(BALANCER_FLASH_LOAN_EVENT.as_bytes())
                    .as_slice()
            )
        );
    }

    #[test]
    fn decodes_aave_flash_loan() {
        let mut log = Log::default();
        let receiver = Address::from_low_u64_be(0x11);
        let initiator = Address::from_low_u64_be(0x22);
        let asset = Address::from_low_u64_be(0x33);
        log.topics = vec![
            event_topic_from_signature(AAVE_FLASH_LOAN_EVENT),
            topic_for_address(receiver),
            topic_for_address(initiator),
            topic_for_address(asset),
        ];
        log.data = encode_words(&[
            U256::from(1_000_000_u64), // amount
            U256::from(2_u64),         // interestRateMode
            U256::from(900_u64),       // premium
            U256::from(0_u64),         // referralCode
        ]);

        let decoded = decode_aave_flash_loan(&log).expect("aave flash loan should decode");
        assert_eq!(decoded.asset_address, Some(asset));
        assert_eq!(decoded.loan_amount_raw, U256::from(1_000_000_u64));
        assert_eq!(decoded.fee_raw, Some(U256::from(900_u64)));
        assert_eq!(decoded.borrower, Some(receiver));
        assert_eq!(decoded.initiator, Some(initiator));
    }

    #[test]
    fn decodes_balancer_flash_loan() {
        let mut log = Log::default();
        let recipient = Address::from_low_u64_be(0x44);
        let token = Address::from_low_u64_be(0x55);
        log.topics = vec![
            event_topic_from_signature(BALANCER_FLASH_LOAN_EVENT),
            topic_for_address(recipient),
            topic_for_address(token),
        ];
        log.data = encode_words(&[U256::from(2_000_000_u64), U256::from(250_u64)]);

        let decoded =
            decode_flash_loan_log("balancer-v2", &log).expect("balancer flash loan should decode");
        assert_eq!(decoded.asset_address, Some(token));
        assert_eq!(decoded.loan_amount_raw, U256::from(2_000_000_u64));
        assert_eq!(decoded.fee_raw, Some(U256::from(250_u64)));
        assert_eq!(decoded.borrower, Some(recipient));
        assert_eq!(decoded.initiator, None);
    }

    fn topic_for_address(address: Address) -> H256 {
        let mut bytes = [0_u8; 32];
        bytes[12..].copy_from_slice(address.as_bytes());
        H256::from_slice(&bytes)
    }

    fn encode_words(words: &[U256]) -> ethers::types::Bytes {
        let mut data = Vec::with_capacity(words.len() * 32);
        for word in words {
            let mut slot = [0_u8; 32];
            word.to_big_endian(&mut slot);
            data.extend_from_slice(&slot);
        }
        data.into()
    }
}

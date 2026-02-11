use ethers::{
    types::{Log, H256, I256, U256},
    utils::keccak256,
};

pub const ANSWER_UPDATED_EVENT: &str = "AnswerUpdated(int256,uint256,uint256)";

pub fn answer_updated_topic() -> H256 {
    H256::from_slice(keccak256(ANSWER_UPDATED_EVENT.as_bytes()).as_slice())
}

pub fn decode_answer_updated_raw(log: &Log) -> Option<I256> {
    if log.topics.len() < 3 {
        return None;
    }

    Some(I256::from_raw(U256::from_big_endian(
        log.topics[1].as_bytes(),
    )))
}

pub fn extract_round_id(log: &Log) -> Option<U256> {
    if log.topics.len() < 3 {
        return None;
    }

    Some(U256::from_big_endian(log.topics[2].as_bytes()))
}

pub fn scale_answer(raw: &I256, decimals: u8) -> f64 {
    raw.to_string().parse::<f64>().unwrap_or_default() / 10_f64.powi(decimals as i32)
}

#[cfg(test)]
mod tests {
    use ethers::types::{Log, U256};

    use super::*;

    #[test]
    fn decodes_and_scales_answer_updated_topic_value() {
        let mut log = Log::default();
        let current = U256::from(123_450_000_000_u64); // 1234.5 @ 8 decimals
        let round_id = U256::from(42_u64);
        let mut current_bytes = [0_u8; 32];
        current.to_big_endian(&mut current_bytes);
        let mut round_bytes = [0_u8; 32];
        round_id.to_big_endian(&mut round_bytes);
        log.topics = vec![
            answer_updated_topic(),
            H256::from_slice(&current_bytes),
            H256::from_slice(&round_bytes),
        ];

        let raw = decode_answer_updated_raw(&log).expect("answer should decode");
        let scaled = scale_answer(&raw, 8);

        assert!((scaled - 1234.5).abs() < f64::EPSILON);
        assert_eq!(extract_round_id(&log), Some(round_id));
    }
}

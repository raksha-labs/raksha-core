use anyhow::Result;
use common_types::DetectionResult;

pub fn enrich_with_trace(mut result: DetectionResult) -> Result<DetectionResult> {
    result.oracle_context.insert(
        "trace_summary".to_string(),
        serde_json::json!({
            "has_flash_loan": true,
            "same_oracle_read_twice": true,
            "large_swap_detected": true,
            "source": "mock-trace-enricher"
        }),
    );
    Ok(result)
}

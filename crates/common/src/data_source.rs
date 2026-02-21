use serde_json::Value;

/// Configuration for a data source loaded from the database.
/// Used by both state-manager (to load configs) and ingestion (to create connectors).
#[derive(Debug, Clone)]
pub struct DataSourceConfig {
    pub tenant_id: String,
    pub source_id: String,
    pub source_type: String,
    pub source_name: String,
    pub connection_config: Value,
    /// Optional tenant-level filters (contract addresses, market pairs…).
    pub filters: Option<Value>,
    pub enabled: bool,
}

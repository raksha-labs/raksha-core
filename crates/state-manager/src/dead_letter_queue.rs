/// Dead Letter Queue for failed event processing
///
/// Stores events that repeatedly fail processing for later investigation.
/// Prevents event loss while avoiding infinite retry loops.
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_postgres::Client;
use tracing::{info, warn};

/// Maximum number of retry attempts before sending to DLQ
const DEFAULT_MAX_RETRIES: i32 = 3;

/// Dead letter queue entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetterEntry {
    pub id: String,
    pub stream_name: String,
    pub entry_id: String,
    pub payload: serde_json::Value,
    pub error_message: String,
    pub retry_count: i32,
    pub first_failure_at: DateTime<Utc>,
    pub last_failure_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// Dead Letter Queue for managing failed events
pub struct DeadLetterQueue {
    client: Client,
    max_retries: i32,
}

impl DeadLetterQueue {
    /// Create a new DLQ with the provided PostgreSQL client
    pub fn new(client: Client) -> Self {
        Self {
            client,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }

    /// Create a new DLQ with custom max retries
    pub fn with_max_retries(client: Client, max_retries: i32) -> Self {
        Self {
            client,
            max_retries,
        }
    }

    /// Initialize the DLQ schema
    pub async fn init_schema(&self) -> Result<()> {
        let exists = self
            .client
            .query_opt(
                r#"
                SELECT 1
                FROM information_schema.tables
                WHERE table_schema = 'public'
                  AND table_name = 'dead_letter_queue'
                "#,
                &[],
            )
            .await?;

        if exists.is_none() {
            anyhow::bail!(
                "missing core schema table: dead_letter_queue. Run SQL bootstrap (schema.sql + seed_data.sql)"
            );
        }

        info!("dead letter queue schema validated");
        Ok(())
    }

    /// Record a failed event processing attempt
    ///
    /// Returns true if the event should be sent to DLQ (max retries exceeded)
    pub async fn record_failure(
        &self,
        stream_name: &str,
        entry_id: &str,
        payload: &serde_json::Value,
        error: &str,
    ) -> Result<bool> {
        let id = format!("{}:{}", stream_name, entry_id);

        // Check if entry exists
        let existing = self
            .client
            .query_opt(
                "SELECT retry_count, first_failure_at FROM dead_letter_queue WHERE id = $1",
                &[&id],
            )
            .await?;

        if let Some(row) = existing {
            // Update existing entry
            let retry_count: i32 = row.get(0);
            let _first_failure_at: DateTime<Utc> = row.get(1);
            let new_retry_count = retry_count + 1;

            self.client
                .execute(
                    r#"
                    UPDATE dead_letter_queue
                    SET retry_count = $1, last_failure_at = NOW(), error_message = $2
                    WHERE id = $3
                    "#,
                    &[&new_retry_count, &error, &id],
                )
                .await?;

            if new_retry_count >= self.max_retries {
                warn!(
                    stream = %stream_name,
                    entry_id = %entry_id,
                    retry_count = new_retry_count,
                    "event moved to dead letter queue (max retries exceeded)"
                );
                Ok(true)
            } else {
                info!(
                    stream = %stream_name,
                    entry_id = %entry_id,
                    retry_count = new_retry_count,
                    max_retries = self.max_retries,
                    "event failure recorded, will retry"
                );
                Ok(false)
            }
        } else {
            // Create new entry
            let now = Utc::now();
            self.client
                .execute(
                    r#"
                    INSERT INTO dead_letter_queue
                    (id, stream_name, entry_id, payload, error_message, retry_count, first_failure_at, last_failure_at, created_at)
                    VALUES ($1, $2, $3, $4, $5, 1, $6, $6, $6)
                    "#,
                    &[&id, &stream_name, &entry_id, payload, &error, &now],
                )
                .await?;

            info!(
                stream = %stream_name,
                entry_id = %entry_id,
                "first failure recorded"
            );
            Ok(false)
        }
    }

    /// Check if an event should be retried based on DLQ state
    pub async fn should_retry(&self, stream_name: &str, entry_id: &str) -> Result<bool> {
        let id = format!("{}:{}", stream_name, entry_id);

        let row = self
            .client
            .query_opt(
                "SELECT retry_count FROM dead_letter_queue WHERE id = $1",
                &[&id],
            )
            .await?;

        if let Some(row) = row {
            let retry_count: i32 = row.get(0);
            Ok(retry_count < self.max_retries)
        } else {
            // Entry not in DLQ, should process normally
            Ok(true)
        }
    }

    /// Remove an entry from DLQ after successful processing
    pub async fn remove_entry(&self, stream_name: &str, entry_id: &str) -> Result<()> {
        let id = format!("{}:{}", stream_name, entry_id);

        let deleted = self
            .client
            .execute("DELETE FROM dead_letter_queue WHERE id = $1", &[&id])
            .await?;

        if deleted > 0 {
            info!(
                stream = %stream_name,
                entry_id = %entry_id,
                "event recovered from DLQ"
            );
        }

        Ok(())
    }

    /// List entries in DLQ for monitoring
    pub async fn list_entries(&self, limit: i64) -> Result<Vec<DeadLetterEntry>> {
        let rows = self
            .client
            .query(
                r#"
                SELECT id, stream_name, entry_id, payload, error_message, retry_count,
                       first_failure_at, last_failure_at, created_at
                FROM dead_letter_queue
                ORDER BY created_at DESC
                LIMIT $1
                "#,
                &[&limit],
            )
            .await?;

        let mut entries = Vec::new();
        for row in rows {
            entries.push(DeadLetterEntry {
                id: row.get(0),
                stream_name: row.get(1),
                entry_id: row.get(2),
                payload: row.get(3),
                error_message: row.get(4),
                retry_count: row.get(5),
                first_failure_at: row.get(6),
                last_failure_at: row.get(7),
                created_at: row.get(8),
            });
        }

        Ok(entries)
    }

    /// Get DLQ statistics
    pub async fn get_stats(&self) -> Result<DeadLetterStats> {
        let row = self
            .client
            .query_one(
                r#"
                SELECT 
                    COUNT(*) as total_entries,
                    COUNT(CASE WHEN retry_count >= $1 THEN 1 END) as permanent_failures,
                    COUNT(CASE WHEN retry_count < $1 THEN 1 END) as retryable_failures
                FROM dead_letter_queue
                "#,
                &[&self.max_retries],
            )
            .await?;

        Ok(DeadLetterStats {
            total_entries: row.get::<_, i64>(0) as u64,
            permanent_failures: row.get::<_, i64>(1) as u64,
            retryable_failures: row.get::<_, i64>(2) as u64,
        })
    }
}

#[derive(Debug, Clone)]
pub struct DeadLetterStats {
    pub total_entries: u64,
    pub permanent_failures: u64,
    pub retryable_failures: u64,
}

#[cfg(test)]
mod tests {
    // Note: These tests require a PostgreSQL instance
    // In a real project, use testcontainers or similar for integration tests
}

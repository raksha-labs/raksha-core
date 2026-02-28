use anyhow::{Context, Result};
use async_trait::async_trait;
use event_schema::{
    AlertEvent, DetectionResult, FinalityUpdate, NormalizedEvent, ReorgNotice, UnifiedEvent,
};
use common::EventBus;
use redis::{streams::StreamReadReply, FromRedisValue};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::info;

mod correlation;
mod finality;
mod persistence;
mod dead_letter_queue;

pub use correlation::*;
pub use finality::*;
pub use persistence::*;
pub use dead_letter_queue::*;

pub const STREAM_NORMALIZED_EVENTS: &str = "raksha:normalized-events";
pub const STREAM_REORG_NOTICES: &str = "raksha:reorg-notices";
pub const STREAM_FINALITY_UPDATES: &str = "raksha:finality-updates";
pub const STREAM_DETECTIONS: &str = "raksha:detections";
pub const STREAM_ALERTS: &str = "raksha:alerts";
pub const STREAM_ALERT_LIFECYCLE: &str = "raksha:alerts:lifecycle";
pub const STREAM_UNIFIED_EVENTS: &str = "raksha:unified-events";

#[derive(Clone)]
pub struct RedisStreamPublisher {
    client: redis::Client,
    normalized_events_stream: String,
    reorg_notices_stream: String,
    finality_updates_stream: String,
    detection_stream: String,
    alert_stream: String,
    alert_lifecycle_stream: String,
    unified_events_stream: String,
}

impl RedisStreamPublisher {
    pub fn from_url(redis_url: &str) -> Result<Self> {
        Ok(Self {
            client: redis::Client::open(redis_url)?,
            normalized_events_stream: STREAM_NORMALIZED_EVENTS.to_string(),
            reorg_notices_stream: STREAM_REORG_NOTICES.to_string(),
            finality_updates_stream: STREAM_FINALITY_UPDATES.to_string(),
            detection_stream: STREAM_DETECTIONS.to_string(),
            alert_stream: STREAM_ALERTS.to_string(),
            alert_lifecycle_stream: STREAM_ALERT_LIFECYCLE.to_string(),
            unified_events_stream: STREAM_UNIFIED_EVENTS.to_string(),
        })
    }

    pub fn from_env() -> Option<Result<Self>> {
        std::env::var("REDIS_URL")
            .ok()
            .map(|url| Self::from_url(&url))
    }

    pub async fn publish_normalized_event(&self, event: &NormalizedEvent) -> Result<()> {
        self.publish_struct(&self.normalized_events_stream, event)
            .await
    }

    pub async fn publish_reorg_notice(&self, notice: &ReorgNotice) -> Result<()> {
        self.publish_struct(&self.reorg_notices_stream, notice)
            .await
    }

    pub async fn publish_finality_update(&self, update: &FinalityUpdate) -> Result<()> {
        self.publish_struct(&self.finality_updates_stream, update)
            .await
    }

    pub async fn publish_detection(&self, detection: &DetectionResult) -> Result<()> {
        self.publish_struct(&self.detection_stream, detection).await
    }

    pub async fn publish_alert(&self, alert: &AlertEvent) -> Result<()> {
        self.publish_struct(&self.alert_stream, alert).await
    }

    pub async fn publish_alert_lifecycle(&self, alert: &AlertEvent) -> Result<()> {
        self.publish_struct(&self.alert_lifecycle_stream, alert)
            .await
    }

    pub async fn publish_unified_event(&self, event: &UnifiedEvent) -> Result<()> {
        self.publish_struct(&self.unified_events_stream, event).await
    }

    pub async fn healthcheck(&self) -> Result<()> {
        let mut connection = self.client.get_multiplexed_async_connection().await?;
        let pong: String = redis::cmd("PING").query_async(&mut connection).await?;
        info!(pong = %pong, "redis healthcheck passed");
        Ok(())
    }

    pub async fn publish_raw_json(&self, stream: &str, payload: &Value) -> Result<()> {
        let payload = serde_json::to_string(payload)?;
        let mut connection = self.client.get_multiplexed_async_connection().await?;
        let _: String = redis::cmd("XADD")
            .arg(stream)
            .arg("*")
            .arg("payload")
            .arg(payload)
            .query_async(&mut connection)
            .await?;
        Ok(())
    }

    pub async fn read_normalized_events(
        &self,
        last_id: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<(String, NormalizedEvent)>> {
        self.read_payloads(&self.normalized_events_stream, last_id, count, block_ms)
            .await
    }

    pub async fn read_detections(
        &self,
        last_id: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<(String, DetectionResult)>> {
        self.read_payloads(&self.detection_stream, last_id, count, block_ms)
            .await
    }

    pub async fn read_reorg_notices(
        &self,
        last_id: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<(String, ReorgNotice)>> {
        self.read_payloads(&self.reorg_notices_stream, last_id, count, block_ms)
            .await
    }

    pub async fn read_finality_updates(
        &self,
        last_id: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<(String, FinalityUpdate)>> {
        self.read_payloads(&self.finality_updates_stream, last_id, count, block_ms)
            .await
    }

    pub async fn read_unified_events(
        &self,
        last_id: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<(String, UnifiedEvent)>> {
        self.read_payloads(&self.unified_events_stream, last_id, count, block_ms)
            .await
    }

    pub async fn ensure_normalized_events_group(&self, group: &str) -> Result<()> {
        self.ensure_consumer_group(&self.normalized_events_stream, group)
            .await
    }

    pub async fn ensure_reorg_notices_group(&self, group: &str) -> Result<()> {
        self.ensure_consumer_group(&self.reorg_notices_stream, group)
            .await
    }

    pub async fn ensure_detections_group(&self, group: &str) -> Result<()> {
        self.ensure_consumer_group(&self.detection_stream, group)
            .await
    }

    pub async fn ensure_finality_updates_group(&self, group: &str) -> Result<()> {
        self.ensure_consumer_group(&self.finality_updates_stream, group)
            .await
    }

    pub async fn ensure_unified_events_group(&self, group: &str) -> Result<()> {
        self.ensure_consumer_group(&self.unified_events_stream, group)
            .await
    }

    pub async fn read_normalized_events_group(
        &self,
        group: &str,
        consumer: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<(String, NormalizedEvent)>> {
        self.read_group_payloads(
            &self.normalized_events_stream,
            group,
            consumer,
            count,
            block_ms,
        )
        .await
    }

    pub async fn read_detections_group(
        &self,
        group: &str,
        consumer: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<(String, DetectionResult)>> {
        self.read_group_payloads(&self.detection_stream, group, consumer, count, block_ms)
            .await
    }

    pub async fn read_reorg_notices_group(
        &self,
        group: &str,
        consumer: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<(String, ReorgNotice)>> {
        self.read_group_payloads(&self.reorg_notices_stream, group, consumer, count, block_ms)
            .await
    }

    pub async fn read_finality_updates_group(
        &self,
        group: &str,
        consumer: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<(String, FinalityUpdate)>> {
        self.read_group_payloads(
            &self.finality_updates_stream,
            group,
            consumer,
            count,
            block_ms,
        )
        .await
    }

    pub async fn read_unified_events_group(
        &self,
        group: &str,
        consumer: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<(String, UnifiedEvent)>> {
        self.read_group_payloads(&self.unified_events_stream, group, consumer, count, block_ms)
            .await
    }

    pub async fn ack_normalized_event(&self, group: &str, entry_id: &str) -> Result<()> {
        self.ack_entry(&self.normalized_events_stream, group, entry_id)
            .await
    }

    pub async fn ack_detection(&self, group: &str, entry_id: &str) -> Result<()> {
        self.ack_entry(&self.detection_stream, group, entry_id)
            .await
    }

    pub async fn ack_reorg_notice(&self, group: &str, entry_id: &str) -> Result<()> {
        self.ack_entry(&self.reorg_notices_stream, group, entry_id)
            .await
    }

    pub async fn ack_finality_update(&self, group: &str, entry_id: &str) -> Result<()> {
        self.ack_entry(&self.finality_updates_stream, group, entry_id)
            .await
    }

    pub async fn ack_unified_event(&self, group: &str, entry_id: &str) -> Result<()> {
        self.ack_entry(&self.unified_events_stream, group, entry_id)
            .await
    }

    async fn publish_struct<T: serde::Serialize>(&self, stream: &str, value: &T) -> Result<()> {
        let payload = serde_json::to_value(value)?;
        self.publish_raw_json(stream, &payload).await?;
        Ok(())
    }

    async fn read_payloads<T: DeserializeOwned>(
        &self,
        stream: &str,
        last_id: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<(String, T)>> {
        let mut connection = self.client.get_multiplexed_async_connection().await?;
        let reply: Option<StreamReadReply> = redis::cmd("XREAD")
            .arg("BLOCK")
            .arg(block_ms)
            .arg("COUNT")
            .arg(count.max(1))
            .arg("STREAMS")
            .arg(stream)
            .arg(last_id)
            .query_async(&mut connection)
            .await?;

        let Some(reply) = reply else {
            return Ok(Vec::new());
        };

        let mut entries = Vec::new();
        for stream_key in reply.keys {
            for stream_id in stream_key.ids {
                let Some(redis_value) = stream_id.map.get("payload") else {
                    continue;
                };
                let payload: String = FromRedisValue::from_redis_value(redis_value)
                    .context("failed to parse stream payload as string")?;
                let value = serde_json::from_str::<T>(&payload).with_context(|| {
                    format!("failed to deserialize payload for stream {stream}")
                })?;
                entries.push((stream_id.id, value));
            }
        }

        Ok(entries)
    }

    async fn ensure_consumer_group(&self, stream: &str, group: &str) -> Result<()> {
        let mut connection = self.client.get_multiplexed_async_connection().await?;
        let create_result: redis::RedisResult<String> = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(stream)
            .arg(group)
            .arg("0")
            .arg("MKSTREAM")
            .query_async(&mut connection)
            .await;

        match create_result {
            Ok(_) => Ok(()),
            Err(err) => {
                if err.to_string().contains("BUSYGROUP") {
                    Ok(())
                } else {
                    Err(err.into())
                }
            }
        }
    }

    async fn read_group_payloads<T: DeserializeOwned>(
        &self,
        stream: &str,
        group: &str,
        consumer: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<(String, T)>> {
        let mut connection = self.client.get_multiplexed_async_connection().await?;
        let reply: Option<StreamReadReply> = redis::cmd("XREADGROUP")
            .arg("GROUP")
            .arg(group)
            .arg(consumer)
            .arg("BLOCK")
            .arg(block_ms)
            .arg("COUNT")
            .arg(count.max(1))
            .arg("STREAMS")
            .arg(stream)
            .arg(">")
            .query_async(&mut connection)
            .await?;

        let Some(reply) = reply else {
            return Ok(Vec::new());
        };

        let mut entries = Vec::new();
        for stream_key in reply.keys {
            for stream_id in stream_key.ids {
                let Some(redis_value) = stream_id.map.get("payload") else {
                    continue;
                };
                let payload: String = FromRedisValue::from_redis_value(redis_value)
                    .context("failed to parse stream payload as string")?;
                let value = serde_json::from_str::<T>(&payload).with_context(|| {
                    format!("failed to deserialize payload for stream {stream}")
                })?;
                entries.push((stream_id.id, value));
            }
        }

        Ok(entries)
    }

    async fn ack_entry(&self, stream: &str, group: &str, entry_id: &str) -> Result<()> {
        let mut connection = self.client.get_multiplexed_async_connection().await?;
        let _: i64 = redis::cmd("XACK")
            .arg(stream)
            .arg(group)
            .arg(entry_id)
            .query_async(&mut connection)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl EventBus for RedisStreamPublisher {
    async fn publish_json(&self, stream: &str, payload: &Value) -> Result<()> {
        self.publish_raw_json(stream, payload).await
    }

    async fn healthcheck(&self) -> Result<()> {
        RedisStreamPublisher::healthcheck(self).await
    }
}

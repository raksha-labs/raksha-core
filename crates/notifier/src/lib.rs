use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use event_schema::AlertEvent;
use common::AlertSink;
use reqwest::Client;
use serde_json::json;
use tracing::{info, warn};

const DEFAULT_NOTIFIER_GATEWAY_URL: &str = "http://localhost:3002";
const DEFAULT_ALERT_FALLBACK_TENANT_ID: &str = "default";

#[derive(Debug, Clone)]
pub struct GatewayChannelResult {
    pub channel: String,
    pub delivered: bool,
    pub reason: Option<String>,
    pub status_code: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct GatewayDispatchResult {
    pub tenant_id: String,
    pub delivered: bool,
    pub reason: Option<String>,
    pub resolved_channels: Vec<String>,
    pub results: Vec<GatewayChannelResult>,
}

#[derive(Clone)]
struct NotifierGatewayHttp {
    client: Client,
    endpoint: String,
}

impl NotifierGatewayHttp {
    fn from_env() -> Self {
        let endpoint = std::env::var("NOTIFIER_GATEWAY_URL")
            .unwrap_or_else(|_| DEFAULT_NOTIFIER_GATEWAY_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
            endpoint,
        }
    }

    async fn dispatch(&self, channel: &str, alert: &AlertEvent) -> Result<()> {
        let tenant_id = alert
            .tenant_id
            .clone()
            .unwrap_or_else(|| DEFAULT_ALERT_FALLBACK_TENANT_ID.to_string());
        let dedup_key = format!(
            "{}:{}:{}:{}",
            format!("{:?}", alert.chain).to_lowercase(),
            alert.protocol,
            channel,
            alert.tx_hash
        );

        let response = self
            .client
            .post(format!("{}/dispatch", self.endpoint))
            .json(&json!({
                "tenant_id": tenant_id,
                "dedup_key": dedup_key,
                "requested_channels": [channel],
                "severity": format!("{:?}", alert.severity).to_lowercase(),
                "payload": alert
            }))
            .send()
            .await
            .with_context(|| {
                format!("failed to call notifier-gateway dispatch at {}", self.endpoint)
            })?;

        response.error_for_status().with_context(|| {
            format!(
                "notifier-gateway returned non-success status for channel {}",
                channel
            )
        })?;

        info!(channel, "alert forwarded to notifier-gateway");
        Ok(())
    }
}

#[derive(Clone)]
pub struct NotifierGatewayClient {
    http: NotifierGatewayHttp,
    fallback_tenant_id: String,
}

impl Default for NotifierGatewayClient {
    fn default() -> Self {
        Self::from_env()
    }
}

impl NotifierGatewayClient {
    pub fn from_env() -> Self {
        Self {
            http: NotifierGatewayHttp::from_env(),
            fallback_tenant_id: std::env::var("ALERT_FALLBACK_TENANT_ID")
                .unwrap_or_else(|_| DEFAULT_ALERT_FALLBACK_TENANT_ID.to_string()),
        }
    }

    pub async fn dispatch_alert(&self, alert: &AlertEvent) -> Result<GatewayDispatchResult> {
        let tenant_id = alert
            .tenant_id
            .clone()
            .unwrap_or_else(|| self.fallback_tenant_id.clone());
        let requested_channels = alert
            .channel_routes
            .iter()
            .filter(|channel| {
                matches!(
                    channel.as_str(),
                    "webhook" | "slack" | "telegram" | "discord"
                )
            })
            .cloned()
            .collect::<Vec<_>>();

        let response = self
            .http
            .client
            .post(format!("{}/dispatch", self.http.endpoint))
            .json(&json!({
                "tenant_id": tenant_id,
                "dedup_key": alert.dedup_key,
                "severity": format!("{:?}", alert.severity).to_lowercase(),
                "requested_channels": requested_channels,
                "payload": alert,
            }))
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to call notifier-gateway dispatch at {}",
                    self.http.endpoint
                )
            })?;

        let status = response.status();
        let payload = response
            .json::<serde_json::Value>()
            .await
            .unwrap_or_else(|_| json!({}));
        let result = parse_gateway_dispatch_result(&payload, tenant_id);

        if !status.is_success() {
            warn!(
                status = %status,
                reason = ?result.reason,
                "notifier-gateway returned non-success status"
            );
        }

        Ok(result)
    }
}

/// NotifierGatewaySink sends alert to notifier-gateway once and lets gateway route channels.
pub struct NotifierGatewaySink {
    gateway: NotifierGatewayClient,
}

impl Default for NotifierGatewaySink {
    fn default() -> Self {
        Self {
            gateway: NotifierGatewayClient::from_env(),
        }
    }
}

impl NotifierGatewaySink {
    pub fn with_client(gateway: NotifierGatewayClient) -> Self {
        Self { gateway }
    }
}

fn parse_gateway_dispatch_result(
    value: &serde_json::Value,
    tenant_id_fallback: String,
) -> GatewayDispatchResult {
    let tenant_id = value
        .get("tenant_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or(tenant_id_fallback);
    let delivered = value
        .get("delivered")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let resolved_channels = value
        .get("resolved_channels")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let results = value
        .get("results")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|entry| GatewayChannelResult {
                    channel: entry
                        .get("channel")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    delivered: entry
                        .get("delivered")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    reason: entry
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    status_code: entry
                        .get("status_code")
                        .and_then(serde_json::Value::as_u64)
                        .map(|v| v as u16),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    GatewayDispatchResult {
        tenant_id,
        delivered,
        reason,
        resolved_channels,
        results,
    }
}

/// WebhookSink sends alerts to a custom webhook URL
pub struct WebhookSink {
    http: NotifierGatewayHttp,
    client: Client,
    webhook_url: Option<String>,
    direct_mode: bool,
}

/// SlackSink sends alerts to Slack webhook
pub struct SlackSink {
    http: NotifierGatewayHttp,
    client: Client,
    webhook_url: Option<String>,
    direct_mode: bool,
}

/// TelegramSink sends alerts to Telegram bot
pub struct TelegramSink {
    http: NotifierGatewayHttp,
    client: Client,
    bot_token: Option<String>,
    chat_id: Option<String>,
    direct_mode: bool,
}

/// DiscordSink sends alerts to Discord webhook
pub struct DiscordSink {
    http: NotifierGatewayHttp,
    client: Client,
    webhook_url: Option<String>,
    direct_mode: bool,
}

impl Default for WebhookSink {
    fn default() -> Self {
        Self::new()
    }
}

impl WebhookSink {
    pub fn new() -> Self {
        let webhook_url = std::env::var("WEBHOOK_URL").ok();
        let direct_mode = webhook_url.is_some();

        Self {
            http: NotifierGatewayHttp::from_env(),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
            webhook_url,
            direct_mode,
        }
    }

    async fn send_direct(&self, alert: &AlertEvent) -> Result<()> {
        let url = self
            .webhook_url
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No webhook URL configured"))?;

        let payload = json!({
            "alert_id": alert.alert_id,
            "chain": format!("{:?}", alert.chain),
            "protocol": alert.protocol,
            "severity": format!("{:?}", alert.severity),
            "risk_score": alert.risk_score,
            "tx_hash": alert.tx_hash,
            "block_number": alert.block_number,
            "actions_recommended": alert.actions_recommended,
            "created_at": alert.created_at,
        });

        self.client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("Failed to send webhook")?
            .error_for_status()
            .context("Webhook returned error status")?;

        info!("Alert sent to webhook: {}", url);
        Ok(())
    }
}

impl Default for SlackSink {
    fn default() -> Self {
        Self::new()
    }
}

impl SlackSink {
    pub fn new() -> Self {
        let webhook_url = std::env::var("SLACK_WEBHOOK_URL").ok();
        let direct_mode = webhook_url.is_some();

        Self {
            http: NotifierGatewayHttp::from_env(),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
            webhook_url,
            direct_mode,
        }
    }

    async fn send_direct(&self, alert: &AlertEvent) -> Result<()> {
        let url = self
            .webhook_url
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No Slack webhook URL configured"))?;

        let severity_emoji = match alert.severity {
            event_schema::Severity::Critical => "🔴",
            event_schema::Severity::High => "🟠",
            event_schema::Severity::Medium => "🟡",
            event_schema::Severity::Low => "🟢",
            event_schema::Severity::Info => "ℹ️",
        };

        let payload = json!({
            "text": format!("{} Raksha Alert: {} on {}",
                severity_emoji, alert.protocol, format!("{:?}", alert.chain)),
            "blocks": [
                {
                    "type": "header",
                    "text": {
                        "type": "plain_text",
                        "text": format!("{} {} Alert", severity_emoji, format!("{:?}", alert.severity)),
                    }
                },
                {
                    "type": "section",
                    "fields": [
                        {
                            "type": "mrkdwn",
                            "text": format!("*Protocol:*\n{}", alert.protocol)
                        },
                        {
                            "type": "mrkdwn",
                            "text": format!("*Chain:*\n{:?}", alert.chain)
                        },
                        {
                            "type": "mrkdwn",
                            "text": format!("*Risk Score:*\n{}/100", alert.risk_score)
                        },
                        {
                            "type": "mrkdwn",
                            "text": format!("*Block:*\n{}", alert.block_number)
                        }
                    ]
                },
                {
                    "type": "section",
                    "text": {
                        "type": "mrkdwn",
                        "text": format!("*Transaction:*\n`{}`", alert.tx_hash)
                    }
                },
                {
                    "type": "section",
                    "text": {
                        "type": "mrkdwn",
                        "text": format!("*Actions:*\n{}", alert.actions_recommended.join("\n• "))
                    }
                }
            ]
        });

        self.client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("Failed to send Slack message")?
            .error_for_status()
            .context("Slack returned error status")?;

        info!("Alert sent to Slack");
        Ok(())
    }
}

impl Default for TelegramSink {
    fn default() -> Self {
        Self::new()
    }
}

impl TelegramSink {
    pub fn new() -> Self {
        let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").ok();
        let chat_id = std::env::var("TELEGRAM_CHAT_ID").ok();
        let direct_mode = bot_token.is_some() && chat_id.is_some();

        Self {
            http: NotifierGatewayHttp::from_env(),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
            bot_token,
            chat_id,
            direct_mode,
        }
    }

    async fn send_direct(&self, alert: &AlertEvent) -> Result<()> {
        let bot_token = self
            .bot_token
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No Telegram bot token configured"))?;
        let chat_id = self
            .chat_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No Telegram chat ID configured"))?;

        let severity_emoji = match alert.severity {
            event_schema::Severity::Critical => "🔴",
            event_schema::Severity::High => "🟠",
            event_schema::Severity::Medium => "🟡",
            event_schema::Severity::Low => "🟢",
            event_schema::Severity::Info => "ℹ️",
        };

        let message = format!(
            "{} <b>{} Alert</b>\n\n\
            <b>Protocol:</b> {}\n\
            <b>Chain:</b> {:?}\n\
            <b>Risk Score:</b> {}/100\n\
            <b>Block:</b> {}\n\
            <b>Transaction:</b> <code>{}</code>\n\n\
            <b>Actions:</b>\n{}",
            severity_emoji,
            format!("{:?}", alert.severity),
            alert.protocol,
            alert.chain,
            alert.risk_score,
            alert.block_number,
            alert.tx_hash,
            alert
                .actions_recommended
                .iter()
                .map(|a| format!("• {}", a))
                .collect::<Vec<_>>()
                .join("\n")
        );

        let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);

        let payload = json!({
            "chat_id": chat_id,
            "text": message,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });

        self.client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("Failed to send Telegram message")?
            .error_for_status()
            .context("Telegram returned error status")?;

        info!("Alert sent to Telegram");
        Ok(())
    }
}

impl Default for DiscordSink {
    fn default() -> Self {
        Self::new()
    }
}

impl DiscordSink {
    pub fn new() -> Self {
        let webhook_url = std::env::var("DISCORD_WEBHOOK_URL").ok();
        let direct_mode = webhook_url.is_some();

        Self {
            http: NotifierGatewayHttp::from_env(),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
            webhook_url,
            direct_mode,
        }
    }

    async fn send_direct(&self, alert: &AlertEvent) -> Result<()> {
        let url = self
            .webhook_url
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No Discord webhook URL configured"))?;

        let severity_emoji = match alert.severity {
            event_schema::Severity::Critical => "🔴",
            event_schema::Severity::High => "🟠",
            event_schema::Severity::Medium => "🟡",
            event_schema::Severity::Low => "🟢",
            event_schema::Severity::Info => "ℹ️",
        };

        let embed = json!({
            "title": format!("{} {:?} Alert", severity_emoji, alert.severity),
            "description": format!(
                "**Protocol:** {}\n**Chain:** {:?}\n**Risk Score:** {:.2}/100\n**Confidence:** {:.2}\n**Tx:** `{}`",
                alert.protocol,
                alert.chain,
                alert.risk_score,
                alert.confidence,
                alert.tx_hash
            ),
            "fields": [
                {
                    "name": "Lifecycle",
                    "value": format!("{:?}", alert.lifecycle_state),
                    "inline": true
                },
                {
                    "name": "Block",
                    "value": alert.block_number.to_string(),
                    "inline": true
                }
            ]
        });

        self.client
            .post(url)
            .json(&json!({
                "content": format!("Raksha Alert for {}", alert.protocol),
                "embeds": [embed]
            }))
            .send()
            .await
            .context("Failed to send Discord message")?
            .error_for_status()
            .context("Discord returned error status")?;

        info!("Alert sent to Discord");
        Ok(())
    }
}

#[async_trait]
impl AlertSink for NotifierGatewaySink {
    async fn send(&self, alert: &AlertEvent) -> Result<()> {
        let result = self.gateway.dispatch_alert(alert).await?;
        if result.delivered {
            Ok(())
        } else {
            Err(anyhow!(
                "notifier-gateway dispatch failed for tenant {}: {}",
                result.tenant_id,
                result
                    .reason
                    .unwrap_or_else(|| "all_channels_failed".to_string())
            ))
        }
    }

    fn sink_name(&self) -> &'static str {
        "notifier-gateway"
    }
}

#[async_trait]
impl AlertSink for WebhookSink {
    async fn send(&self, alert: &AlertEvent) -> Result<()> {
        if self.direct_mode {
            self.send_direct(alert).await
        } else {
            self.http.dispatch("webhook", alert).await
        }
    }

    fn sink_name(&self) -> &'static str {
        "webhook"
    }
}

#[async_trait]
impl AlertSink for SlackSink {
    async fn send(&self, alert: &AlertEvent) -> Result<()> {
        if self.direct_mode {
            match self.send_direct(alert).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    warn!(
                        "Failed to send Slack alert directly, falling back to notifier-gateway: {:?}",
                        e
                    );
                    self.http.dispatch("slack", alert).await
                }
            }
        } else {
            self.http.dispatch("slack", alert).await
        }
    }

    fn sink_name(&self) -> &'static str {
        "slack"
    }
}

#[async_trait]
impl AlertSink for TelegramSink {
    async fn send(&self, alert: &AlertEvent) -> Result<()> {
        if self.direct_mode {
            match self.send_direct(alert).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    warn!("Failed to send Telegram alert directly, falling back to notifier-gateway: {:?}", e);
                    self.http.dispatch("telegram", alert).await
                }
            }
        } else {
            self.http.dispatch("telegram", alert).await
        }
    }

    fn sink_name(&self) -> &'static str {
        "telegram"
    }
}

#[async_trait]
impl AlertSink for DiscordSink {
    async fn send(&self, alert: &AlertEvent) -> Result<()> {
        if self.direct_mode {
            match self.send_direct(alert).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    warn!(
                        "Failed to send Discord alert directly, falling back to notifier-gateway: {:?}",
                        e
                    );
                    self.http.dispatch("discord", alert).await
                }
            }
        } else {
            self.http.dispatch("discord", alert).await
        }
    }

    fn sink_name(&self) -> &'static str {
        "discord"
    }
}

use anyhow::{Context, Result};
use async_trait::async_trait;
use common_types::AlertEvent;
use core_interfaces::AlertSink;
use reqwest::Client;
use serde_json::json;
use tracing::{info, warn};

const DEFAULT_ALERT_MANAGER_URL: &str = "http://localhost:3002";

#[derive(Clone)]
struct AlertManagerHttp {
    client: Client,
    endpoint: String,
}

impl AlertManagerHttp {
    fn from_env() -> Self {
        let endpoint = std::env::var("ALERT_MANAGER_URL")
            .unwrap_or_else(|_| DEFAULT_ALERT_MANAGER_URL.to_string())
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
                "dedup_key": dedup_key,
                "channel": channel,
                "severity": format!("{:?}", alert.severity).to_lowercase(),
                "payload": alert
            }))
            .send()
            .await
            .with_context(|| {
                format!("failed to call alert-manager dispatch at {}", self.endpoint)
            })?;

        response.error_for_status().with_context(|| {
            format!(
                "alert-manager returned non-success status for channel {}",
                channel
            )
        })?;

        info!(channel, "alert forwarded to alert-manager");
        Ok(())
    }
}

/// WebhookSink sends alerts to a custom webhook URL
pub struct WebhookSink {
    http: AlertManagerHttp,
    client: Client,
    webhook_url: Option<String>,
    direct_mode: bool,
}

/// SlackSink sends alerts to Slack webhook
pub struct SlackSink {
    http: AlertManagerHttp,
    client: Client,
    webhook_url: Option<String>,
    direct_mode: bool,
}

/// TelegramSink sends alerts to Telegram bot
pub struct TelegramSink {
    http: AlertManagerHttp,
    client: Client,
    bot_token: Option<String>,
    chat_id: Option<String>,
    direct_mode: bool,
}

/// DiscordSink sends alerts to Discord webhook
pub struct DiscordSink {
    http: AlertManagerHttp,
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
            http: AlertManagerHttp::from_env(),
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
            http: AlertManagerHttp::from_env(),
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
            common_types::Severity::Critical => "🔴",
            common_types::Severity::High => "🟠",
            common_types::Severity::Medium => "🟡",
            common_types::Severity::Low => "🟢",
            common_types::Severity::Info => "ℹ️",
        };

        let payload = json!({
            "text": format!("{} DeFi Alert: {} on {}",
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
            http: AlertManagerHttp::from_env(),
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
            common_types::Severity::Critical => "🔴",
            common_types::Severity::High => "🟠",
            common_types::Severity::Medium => "🟡",
            common_types::Severity::Low => "🟢",
            common_types::Severity::Info => "ℹ️",
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
            http: AlertManagerHttp::from_env(),
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
            common_types::Severity::Critical => "🔴",
            common_types::Severity::High => "🟠",
            common_types::Severity::Medium => "🟡",
            common_types::Severity::Low => "🟢",
            common_types::Severity::Info => "ℹ️",
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
                "content": format!("DeFi Surveillance Alert for {}", alert.protocol),
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
                        "Failed to send Slack alert directly, falling back to alert-manager: {:?}",
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
                    warn!("Failed to send Telegram alert directly, falling back to alert-manager: {:?}", e);
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
                        "Failed to send Discord alert directly, falling back to alert-manager: {:?}",
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

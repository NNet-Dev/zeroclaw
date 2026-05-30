//! Microsoft Teams channel implementation for ZeroClaw.
//!
//! Uses the Bot Framework REST API for outbound messages and an HTTP webhook
//! for inbound activities.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use zeroclaw_api::channel::{
    Channel, ChannelApprovalRequest, ChannelApprovalResponse, ChannelMessage, SendMessage,
};

/// Incoming Bot Framework Activity from the webhook.
#[derive(Debug, Deserialize)]
struct TeamsActivity {
    #[serde(rename = "type")]
    activity_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    from: Option<TeamsFrom>,
    #[serde(default)]
    conversation: Option<TeamsConversation>,
    #[serde(default)]
    service_url: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "channelId")]
    channel_id: Option<String>,
    #[serde(default, rename = "replyToId")]
    reply_to_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TeamsFrom {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TeamsConversation {
    id: String,
    #[serde(default, rename = "conversationType")]
    conversation_type: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// Cached access token with expiry.
struct CachedToken {
    token: String,
    expires_at: std::time::Instant,
}

pub struct TeamsChannel {
    client_id: String,
    client_secret: String,
    tenant_id: String,
    service_url: String,
    port: u16,
    alias: String,
    mention_only: bool,
    http_client: Client,
    pending_approvals: Arc<AsyncMutex<HashMap<String, oneshot::Sender<ChannelApprovalResponse>>>>,
    approval_timeout_secs: u64,
    cached_token: Arc<AsyncMutex<Option<CachedToken>>>,
}

impl TeamsChannel {
    pub fn new(
        client_id: String,
        client_secret: String,
        tenant_id: String,
        service_url: String,
        port: u16,
        alias: String,
        mention_only: bool,
    ) -> Self {
        Self {
            client_id,
            client_secret,
            tenant_id,
            service_url,
            port,
            alias,
            mention_only,
            http_client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            pending_approvals: Arc::new(AsyncMutex::new(HashMap::new())),
            approval_timeout_secs: 300,
            cached_token: Arc::new(AsyncMutex::new(None)),
        }
    }

    pub fn with_approval_timeout_secs(mut self, timeout: u64) -> Self {
        self.approval_timeout_secs = timeout;
        self
    }

    async fn get_access_token(&self) -> Result<String> {
        // Check cache first
        {
            let guard = self.cached_token.lock().await;
            if let Some(cached) = guard.as_ref() {
                if cached.expires_at > std::time::Instant::now() {
                    return Ok(cached.token.clone());
                }
            }
        }

        let token_url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant_id
        );

        let params = [
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("scope", "https://api.botframework.com/.default"),
        ];

        let response = self
            .http_client
            .post(&token_url)
            .form(&params)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send()
            .await
            .context("Failed to make token request")?;

        let json: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse token response JSON")?;

        if let Some(token) = json.get("access_token").and_then(serde_json::Value::as_str) {
            let expires_in = json
                .get("expires_in")
                .and_then(|v| v.as_u64())
                .unwrap_or(3600);
            let expires_at =
                std::time::Instant::now() + Duration::from_secs(expires_in.saturating_sub(300));
            let bearer = format!("Bearer {token}");
            *self.cached_token.lock().await = Some(CachedToken {
                token: bearer.clone(),
                expires_at,
            });
            Ok(bearer)
        } else {
            let error_description = json
                .get("error_description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            bail!("Failed to get access token: {error_description}");
        }
    }

    async fn send_activity(
        &self,
        conversation_id: &str,
        text: &str,
        reply_to_id: Option<&str>,
    ) -> Result<()> {
        let access_token = self.get_access_token().await?;
        let activities_url = format!(
            "{}v3/conversations/{}/activities",
            self.service_url, conversation_id
        );

        let mut activity = serde_json::json!({
            "type": "message",
            "text": text,
            "textFormat": "markdown"
        });

        if let Some(reply_id) = reply_to_id {
            activity["replyToId"] = serde_json::Value::String(reply_id.to_string());
        }

        let resp = self
            .http_client
            .post(&activities_url)
            .header("Authorization", &access_token)
            .header("Content-Type", "application/json")
            .json(&activity)
            .send()
            .await
            .context("Failed to send activity to Teams")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_body = resp.text().await.unwrap_or_default();
            bail!("Send activity failed: {status} - {error_body}");
        }

        Ok(())
    }
}

impl ::zeroclaw_api::attribution::Attributable for TeamsChannel {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Channel(::zeroclaw_api::attribution::ChannelKind::Teams)
    }
    fn alias(&self) -> &str {
        &self.alias
    }
}

#[async_trait]
impl Channel for TeamsChannel {
    fn name(&self) -> &str {
        "teams"
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        let (conversation_id, reply_to_id) = if let Some((c, r)) = message.recipient.split_once(':')
        {
            (c, Some(r))
        } else {
            (message.recipient.as_str(), None)
        };

        let chunks = split_message(&message.content, 15_000);

        for (i, chunk) in chunks.iter().enumerate() {
            let reply = if i == 0 { reply_to_id } else { None };
            self.send_activity(conversation_id, chunk, reply).await?;
        }

        Ok(())
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> Result<()> {
        use axum::{
            extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router,
        };
        use portable_atomic::{AtomicU64, Ordering};

        struct WebhookState {
            tx: tokio::sync::mpsc::Sender<ChannelMessage>,
            alias: String,
            mention_only: bool,
            counter: Arc<AtomicU64>,
        }

        let state = Arc::new(WebhookState {
            tx,
            alias: self.alias.clone(),
            mention_only: self.mention_only,
            counter: Arc::new(AtomicU64::new(0)),
        });

        #[axum::debug_handler]
        async fn handle_webhook(
            State(state): State<Arc<WebhookState>>,
            Json(payload): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            let activity: TeamsActivity = match serde_json::from_value(payload) {
                Ok(a) => a,
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{e}")})),
                        "invalid Teams activity payload"
                    );
                    return StatusCode::BAD_REQUEST;
                }
            };

            if activity.activity_type != "message" {
                return StatusCode::OK;
            }

            let Some(text) = activity.text else {
                return StatusCode::OK;
            };

            if text.is_empty() {
                return StatusCode::OK;
            }

            let Some(from) = &activity.from else {
                return StatusCode::BAD_REQUEST;
            };

            let Some(conversation) = &activity.conversation else {
                return StatusCode::BAD_REQUEST;
            };

            let is_dm = matches!(conversation.conversation_type.as_deref(), Some("personal"));

            if state.mention_only && !is_dm && !text.contains("<at>") {
                return StatusCode::OK;
            }

            let seq = state.counter.fetch_add(1, Ordering::Relaxed);

            #[allow(clippy::cast_possible_truncation)]
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let reply_target = if let Some(reply_id) = &activity.reply_to_id {
                format!("{}:{}", conversation.id, reply_id)
            } else {
                conversation.id.clone()
            };

            let interruption_scope = if is_dm {
                None
            } else {
                Some(format!("teams_{}", conversation.id))
            };

            let sender_name = from.name.clone().unwrap_or_else(|| from.id.clone());

            let msg = ChannelMessage {
                id: format!("teams_{seq}"),
                sender: sender_name,
                reply_target,
                content: text,
                channel: format!("teams_{}", conversation.id),
                channel_alias: Some(state.alias.clone()),
                timestamp,
                thread_ts: activity.id.clone(),
                interruption_scope_id: interruption_scope,
                attachments: Vec::new(),
                subject: None,
            };

            if state.tx.send(msg).await.is_err() {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "channel receiver dropped, cannot dispatch Teams message"
                );
                return StatusCode::INTERNAL_SERVER_ERROR;
            }

            StatusCode::OK
        }

        let app = Router::new()
            .route("/api/messages", post(handle_webhook))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", self.port))
            .await
            .with_context(|| format!("Failed to bind Teams webhook to port {}", self.port))?;

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "alias": self.alias,
                    "port": self.port,
                })
            ),
            "Teams webhook listening"
        );

        axum::serve(listener, app)
            .await
            .context("Teams webhook server failed")
    }

    async fn request_approval(
        &self,
        _recipient: &str,
        _request: &ChannelApprovalRequest,
    ) -> Result<Option<ChannelApprovalResponse>> {
        // Teams doesn't have a native approval flow yet — fall back
        // to the generic send + listen mechanism.
        Ok(None)
    }
}

/// Split a message into chunks at word boundaries.
fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    if text.len() <= max_chars {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_chars {
            chunks.push(remaining.to_string());
            break;
        }

        let break_at = remaining[..max_chars]
            .rfind('\n')
            .or_else(|| remaining[..max_chars].rfind(' '))
            .unwrap_or(max_chars);

        chunks.push(remaining[..break_at].to_string());
        remaining = remaining[break_at..].trim_start();
    }

    chunks
}

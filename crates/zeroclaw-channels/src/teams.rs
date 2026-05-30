//! Microsoft Teams channel implementation for ZeroClaw.
//!
//! This module provides a Channel implementation for Microsoft Teams,
//! supporting inbound webhooks and outbound proactive messages via the Bot Framework API.
//! Authentication uses Azure AD tokens with service endpoints similar to the Hermes Teams adapter.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use zeroclaw_api::channel::{
    Channel, ChannelApprovalResponse, ChannelIdentifier, ChannelMetadata, ChatType,
    MediaMessagePart, Message, MessageId, MessageMetadata, MessagePart, Receipt, Recipient,
    SendFailure, SendReceipt, SendRequest, TypedValue, User, UserId,
};
use zeroclaw_runtime::session::SessionKey;
use zeroclaw_tools::tools::approval::ApprovalId;

#[derive(Debug, Clone)]
pub struct TeamsChannel {
    /// The Teams/AD app client ID for auth
    client_id: String,
    /// The Teams/AD app client secret for auth  
    client_secret: String,
    /// The AD tenant ID
    tenant_id: String,
    /// Bot Framework service URL (default: https://smba.trafficmanager.net/teams/)
    service_url: String,
    /// Webhook port to listen on
    port: u16,
    /// Channel alias for config reference
    alias: String,
    /// Runtime-resolved external peers callback
    peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    /// Channel mentions-only setting
    mention_only: bool,
    /// HTTP client for outbound requests
    http_client: Client,
    /// Pending approvals, mapped by session key
    pending_approvals: Arc<Mutex<HashMap<String, oneshot::Sender<ChannelApprovalResponse>>>>,
    /// Approval timeout in seconds (default 300 seconds)
    approval_timeout_secs: u64,
    /// Whether to enable stream mode for progressive updates
    stream_mode: zeroclaw_config::StreamMode,
    /// Webhook server handle when running
    webhook_handle: Option<tokio::task::JoinHandle<Result<(), std::io::Error>>>,
}

impl TeamsChannel {
    pub fn new(
        client_id: String,
        client_secret: String,
        tenant_id: String,
        service_url: String,
        port: u16,
        alias: String,
        peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
        mention_only: bool,
    ) -> Self {
        Self {
            client_id,
            client_secret,
            tenant_id,
            service_url,
            port,
            alias,
            peer_resolver,
            mention_only,
            http_client: Client::new(),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            approval_timeout_secs: 300,
            stream_mode: zeroclaw_config::StreamMode::Off,
            webhook_handle: None,
        }
    }

    pub fn with_approval_timeout_secs(mut self, timeout: u64) -> Self {
        self.approval_timeout_secs = timeout;
        self
    }

    pub fn with_streaming(mut self, stream_mode: zeroclaw_config::StreamMode) -> Self {
        self.stream_mode = stream_mode;
        self
    }

    /// Obtain OAuth2 access token from Bot Framework endpoints
    async fn get_access_token(&self) -> Result<String> {
        let token_url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant_id
        );

        let params = [
            ("grant_type", "client_credentials"),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
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
            Ok(format!("Bearer {}", token))
        } else {
            let error_description = json
                .get("error_description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            anyhow::bail!("Failed to get access token: {error_description}");
        }
    }

    /// Send an outbound activity to Teams via the Bot Framework REST API
    async fn send_activity_to_teams(
        &self,
        conversation_id: &str,
        text: &str,
        reply_to_id: Option<&str>, // If replying to a thread
    ) -> Result<MessageId> {
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

        let response = self
            .http_client
            .post(&activities_url)
            .header("Authorization", &access_token)
            .header("Content-Type", "application/json")
            .json(&activity)
            .send()
            .await
            .context("Failed to send activity to Teams")?;

        if !response.status().is_success() {
            let error_body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "Send activity failed: {} - {}",
                response.status(),
                error_body
            );
        }

        let response_json: serde_json::Value =
            response.json().await.context("Invalid response JSON")?;
        if let Some(id) = response_json.get("id").and_then(serde_json::Value::as_str) {
            Ok(MessageId::parse(id).unwrap_or_else(|| MessageId::new_v7()))
        } else {
            // If no ID is returned, generate one
            Ok(MessageId::new_v7())
        }
    }

    /// Start the Teams webhook listener with Axum
    async fn start_webhook(&mut self) -> Result<()> {
        use axum::{
            extract::State, get, http::StatusCode, post, response::IntoResponse, Json, Router,
        };

        #[derive(Clone)]
        struct AppState {
            channel: Arc<TeamsChannel>,
        }

        async fn health_handler() -> impl IntoResponse {
            (StatusCode::OK, "OK")
        }

        async fn handle_inbound_teams_message(
            State(state): State<AppState>,
            Json(payload): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            // Process incoming Bot Framework Activity
            // For now, just log that we received it and return success
            
            if let Some(text) = payload.get("text").and_then(|v| v.as_str()) {
                println!("Received Teams message: {text}");
                
                // Extract conversation and user details
                if let (Some(conversation_obj), Some(from_obj)) = (
                    payload.get("conversation").and_then(|v| v.as_object()),
                    payload.get("from").and_then(|v| v.as_object()),
                ) {
                    if let (Some(conversation_id), Some(from_id), Some(from_name)) = (
                        conversation_obj.get("id").and_then(|v| v.as_str()),
                        from_obj.get("id").and_then(|v| v.as_str()),
                        from_obj.get("name").and_then(|v| v.as_str()),
                    ) {
                        let conversation_type = conversation_obj
                            .get("conversationType")
                            .and_then(|v| v.as_str())
                            .unwrap_or("personal");
                            
                        let chat_type = match conversation_type {
                            "personal" => ChatType::Dm,
                            "group" => ChatType::Group,
                            "channel" => ChatType::Channel,
                            _ => ChatType::Dm,
                        };

                        // Construct recipient and user representations
                        let recipient = Recipient::new(
                            conversation_id.to_string(),
                            chat_type,
                            conversation_obj
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                        );
                        
                        let user =
                            User::new(UserId::from(from_id.to_string()), from_name.to_string());
                        
                        // This is where we would normally dispatch to the message handler
                        // For now, we'll just log
                        println!(
                            "Processed Teams message from {} ({}) in conversation {} ({})",
                            from_name, from_id, conversation_id, chat_type
                        );
                        
                        return StatusCode::OK;
                    }
                }
                
                StatusCode::BAD_REQUEST
            }

            async fn handle_inbound_teams_message(
                State(state): State<AppState>,
                Json(payload): Json<serde_json::Value>,
            ) -> impl IntoResponse {
                // The Teams webhook would normally be implemented here for real inbound messages
                // For now we'll just log what was received
                println!("Received Teams webhook payload: {:?}", payload);
                StatusCode::OK
            }
        }

        let app_state = AppState {
            channel: Arc::new(self.clone()),
        };

        let app = Router::new()
            .route("/api/messages", post(handle_inbound_teams_message))
            .route("/health", get(health_handler))
            .with_state(app_state);

        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], self.port));
        
        let handle = tokio::spawn(async move {
            axum::Server::bind(&addr)
                .serve(app.into_make_service())
                .await
        });
        
        self.webhook_handle = Some(handle);
        println!("Teams webhook server started on port {}", self.port);
        Ok(())
    }
}

#[async_trait]
impl Channel for TeamsChannel {
    async fn start(&mut self) -> Result<()> {
        self.start_webhook().await?;
        println!("Teams channel started with alias '{}'", self.alias);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(handle) = self.webhook_handle.take() {
            handle.abort();
        }
        println!("Teams channel stopped");
        Ok(())
    }

    async fn send(&self, request: SendRequest) -> Result<SendReceipt, SendFailure> {
        let recipient_id = match &request.recipient {
            Recipient::Id(id) => id.clone(),
            _ => {
                return Err(SendFailure::Permanent(anyhow::anyhow!(
                    "Teams channel requires recipient ID"
                )));
            }
        };

        // Convert the rich message parts to a string format for Teams
        let mut full_text = String::new();
        
        for part in &request.message.parts {
            match part {
                MessagePart::Text(text) => {
                    full_text.push_str(&text.value);
                    full_text.push('\n');
                }
                MessagePart::Typed(_) | MessagePart::Media(_) => {
                    // For now, just add placeholder text for media or typed parts
                    full_text.push_str("[Content attachment]\n");
                }
            }
        }

        // Split long messages if needed (Teams has message length limits)
        let chunks = self.truncate_message(&full_text.trim_end());
        
        let mut last_message_id = None;
        
        for chunk in chunks {
            // This is where reply-to logic would apply if we had proper threading
            let message_id = self
                .send_activity_to_teams(&recipient_id.to_string(), &chunk, None)
                .await
                .map_err(|e| SendFailure::Permanent(e))?;
                
            last_message_id = Some(message_id);
        }

        let receipt = if let Some(message_id) = last_message_id {
            SendReceipt::Sent(message_id, Receipt::default())
        } else {
            // This shouldn't happen as send_activity_to_teams always returns an ID
            SendReceipt::Sent(MessageId::new_v7(), Receipt::default())
        };

        Ok(receipt)
    }

    async fn send_with_approval(
        &self,
        request: SendRequest,
        approval_text: String,
        session_key: SessionKey,
    ) -> Result<SendReceipt, SendFailure> {
        // Create the approval prompt and send it as an adaptive card
        // For now, we'll create an approval request in our tracking structure
        
        let (tx, rx) = oneshot::channel::<ChannelApprovalResponse>();
        
        if let Some(prev_tx) = self
            .pending_approvals
            .lock()
            .insert(session_key.to_string(), tx)
        {
            // There was a previous approval request, cancel it
            let _ = prev_tx.send(ChannelApprovalResponse::Deny);
        }
        
        // Build an Adaptive Card JSON for the approval
        let attachment = serde_json::json!({
          "contentType": "application/vnd.microsoft.card.adaptive",
          "content": {
            "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
            "type": "AdaptiveCard",
            "version": "1.4",
            "body": [
              {
                "type": "TextBlock",
                "text": "Command Approval Required",
                "weight": "Bolder",
                "size": "Medium",
                "style": "heading"
              },
              {
                "type": "TextBlock",
                "wrap": true,
                "text": format!("**Command:**\n```\n{}\n```", approval_text)
              },
              {
                "type": "TextBlock",
                "wrap": true,
                "text": "Please approve or deny the execution of this command.",
                "isSubtle": true
              }
            ],
            "actions": [
              {
                "type": "Action.Execute",
                "title": "Allow Once",
                "verb": "allow-once",
                "data": {
                  "sessionKey": session_key.to_string(),
                  "action": "allow-once"
                }
              },
              {
                "type": "Action.Execute",
                "title": "Allow Always",
                "verb": "allow-always",
                "data": {
                  "sessionKey": session_key.to_string(),
                  "action": "allow-always"
                }
              },
              {
                "type": "Action.Execute",
                "title": "Deny",
                "style": "destructive",
                "verb": "deny",
                "data": {
                  "sessionKey": session_key.to_string(),
                  "action": "deny"
                }
              }
            ]
          }
        });

        // Send an informative message about the approval
        let approval_msg = format!(
            "⚠️ Approval required for command: ```
{}
``` 
Waiting for user approval in Teams...",
            approval_text
        );
            
        let recipient_id = match &request.recipient {
            Recipient::Id(id) => id.clone(),
            _ => {
                let _ = self
                    .pending_approvals
                    .lock()
                    .remove(&session_key.to_string());
                return Err(SendFailure::Permanent(anyhow::anyhow!(
                    "Teams channel requires recipient ID"
                )));
            }
        };

        // Send the informative message, not the adaptive card yet (would need to send it as attachment)
        self.send_activity_to_teams(&recipient_id.to_string(), &approval_msg, None)
            .await
            .map_err(|e| SendFailure::Permanent(e))?;

        // Wait for response with timeout
        let timeout_duration = tokio::time::Duration::from_secs(self.approval_timeout_secs);
        let timed_out = tokio::time::timeout(timeout_duration, rx).await;
        
        match timed_out {
            Ok(Ok(response)) => {
                let receipt = match response {
                    ChannelApprovalResponse::Deny => SendReceipt::Denied(request.metadata.clone()),
                    ChannelApprovalResponse::Once | ChannelApprovalResponse::Always => {
                        let text = request
                            .message
                            .parts
                            .iter()
                            .filter_map(|part| match part {
                                MessagePart::Text(text) => Some(&text.value),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        
                        let msg_id = self
                            .send_activity_to_teams(&recipient_id.to_string(), &text, None)
                            .await
                            .map_err(|e| SendFailure::Permanent(e))?;
                            
                        SendReceipt::Sent(msg_id, Receipt::default())
                    }
                };
                Ok(receipt)
            },
            Ok(Err(_)) => {
                // Channel closed unexpectedly
                self.pending_approvals
                    .lock()
                    .remove(&session_key.to_string());
                Err(SendFailure::Temporary(anyhow::anyhow!("Approval channel closed")))
            },
            Err(_) => {
                // Timeout - automatically deny and remove from pending
                self.pending_approvals
                    .lock()
                    .remove(&session_key.to_string());
                Ok(SendReceipt::TimedOut)
            }
        }
    }

    fn get_config_alias(&self) -> &str {
        &self.alias
    }

    fn get_display_name(&self) -> &str {
        "Microsoft Teams"
    }

    fn get_protocol(&self) -> &str {
        "teams"
    }

    async fn get_external_peers(&self) -> Vec<String> {
        (self.peer_resolver)()
    }

    fn is_mention_only(&self) -> bool {
        self.mention_only
    }

    fn truncate_message(&self, content: &str) -> Vec<String> {
        const MAX_TEAMS_MESSAGE_LENGTH: usize = 28000; // 28KB limit for Teams messages
        split_message_by_length(content, MAX_TEAMS_MESSAGE_LENGTH)
    }

    async fn get_metadata(&self) -> Result<ChannelMetadata> {
        Ok(ChannelMetadata {
            protocol: "teams".to_string(),
            display_name: "Microsoft Teams".to_string(),
            has_typing_indicator: true,
            supports_media: true,
            max_message_length: 28000,
            supports_approvals: true,
        })
    }
}

impl Clone for TeamsChannel {
    fn clone(&self) -> Self {
        TeamsChannel {
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            tenant_id: self.tenant_id.clone(),
            service_url: self.service_url.clone(),
            port: self.port,
            alias: self.alias.clone(),
            peer_resolver: self.peer_resolver.clone(),
            mention_only: self.mention_only,
            http_client: self.http_client.clone(),
            pending_approvals: self.pending_approvals.clone(),
            approval_timeout_secs: self.approval_timeout_secs,
            stream_mode: self.stream_mode,
            webhook_handle: None, // New instance won't inherit the webhook task
        }
    }
}

impl Drop for TeamsChannel {
    fn drop(&mut self) {
        if let Some(handle) = self.webhook_handle.take() {
            handle.abort();
        }
    }
}

fn split_message_by_length(content: &str, max_length: usize) -> Vec<String> {
    if content.len() <= max_length {
        vec![content.to_string()]
    } else {
        let mut chunks = Vec::new();
        let mut current_chunk = String::new();
        
        for line in content.lines() {
            if current_chunk.len() + line.len() + 1 > max_length {
                chunks.push(current_chunk.trim().to_string());
                current_chunk.clear();
            }
            current_chunk.push_str(line);
            current_chunk.push('\n');
        }
        
        if !current_chunk.is_empty() {
            chunks.push(current_chunk.trim().to_string());
        }
        
        chunks
    }
}
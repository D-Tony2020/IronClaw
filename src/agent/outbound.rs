//! Outbound message routing and delivery.
//!
//! The `OutboundRouter` plans and executes delivery of agent responses
//! back to the correct channel and recipient. It supports:
//!
//! - **Reply mode**: Send response back to the original message's channel
//! - **Push mode**: Proactive delivery (routines, heartbeat alerts)
//!
//! BeforeOutbound hooks are executed before delivery, allowing
//! modification or suppression of outgoing messages.

use std::sync::Arc;

use crate::channels::{ChannelManager, IncomingMessage, OutgoingResponse};
use crate::error::Error;
use crate::hooks::{HookEvent, HookOutcome, HookRegistry};

// ---------------------------------------------------------------------------
// DeliveryPlan — describes where and how to deliver a response
// ---------------------------------------------------------------------------

/// Plan for delivering an outbound message.
///
/// Created by `plan_reply()` or `plan_push()`, executed by `deliver()`.
#[derive(Debug, Clone)]
pub struct DeliveryPlan {
    /// Target channel name (e.g. "wechat", "telegram").
    pub channel: String,

    /// Recipient identifier (user_id, group_id).
    pub to: String,

    /// Thread ID for threaded conversations.
    pub thread_id: Option<String>,

    /// Delivery mode: reply to an original message, or proactive push.
    pub mode: DeliveryMode,
}

/// How the message should be delivered.
#[derive(Debug, Clone)]
pub enum DeliveryMode {
    /// Reply to an incoming message (uses `ChannelManager::respond()`).
    Reply {
        /// The original message being replied to.
        original: IncomingMessage,
    },

    /// Proactive push (uses `ChannelManager::broadcast()`).
    Push,
}

// ---------------------------------------------------------------------------
// OutboundRouter — plans and delivers responses
// ---------------------------------------------------------------------------

/// Routes and delivers agent responses to channels.
///
/// Handles:
/// - BeforeOutbound hook execution (modify/suppress)
/// - Reply-to-original and proactive push delivery modes
/// - Error logging without propagation (best-effort delivery)
pub struct OutboundRouter {
    /// Channel manager for actual message delivery.
    channels: Arc<ChannelManager>,

    /// Hook registry for BeforeOutbound hooks.
    hooks: Arc<HookRegistry>,
}

impl OutboundRouter {
    /// Create a new outbound router.
    pub fn new(channels: Arc<ChannelManager>, hooks: Arc<HookRegistry>) -> Self {
        Self { channels, hooks }
    }

    // -------------------------------------------------------------------
    // Plan constructors
    // -------------------------------------------------------------------

    /// Plan a reply to an incoming message.
    ///
    /// The response goes back to the same channel and recipient.
    /// Uses `ChannelManager::respond()` which routes via channel-specific
    /// metadata (e.g., Telegram chat_id, WeChat openid).
    pub fn plan_reply(message: &IncomingMessage) -> DeliveryPlan {
        let to = message
            .metadata
            .get("signal_target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| message.user_id.clone());

        DeliveryPlan {
            channel: message.channel.clone(),
            to,
            thread_id: message.thread_id.clone(),
            mode: DeliveryMode::Reply {
                original: message.clone(),
            },
        }
    }

    /// Plan a proactive push to a specific channel and user.
    ///
    /// Used by routines, heartbeat alerts, and inter-agent notifications.
    pub fn plan_push(channel: &str, to: &str, thread_id: Option<&str>) -> DeliveryPlan {
        DeliveryPlan {
            channel: channel.to_string(),
            to: to.to_string(),
            thread_id: thread_id.map(|s| s.to_string()),
            mode: DeliveryMode::Push,
        }
    }

    // -------------------------------------------------------------------
    // Delivery execution
    // -------------------------------------------------------------------

    /// Deliver a response according to the plan.
    ///
    /// Executes BeforeOutbound hooks before delivery. If a hook blocks
    /// the response, delivery is silently skipped.
    ///
    /// Empty responses are suppressed (not sent to channel).
    pub async fn deliver(
        &self,
        plan: &DeliveryPlan,
        response: String,
    ) -> Result<(), Error> {
        // Suppress empty responses
        if response.is_empty() {
            tracing::debug!(
                channel = %plan.channel,
                to = %plan.to,
                "Suppressed empty response (not sent to channel)"
            );
            return Ok(());
        }

        // Run BeforeOutbound hook
        let event = HookEvent::Outbound {
            user_id: plan.to.clone(),
            channel: plan.channel.clone(),
            content: response.clone(),
            thread_id: plan.thread_id.clone(),
        };

        let final_content = match self.hooks.run(&event).await {
            Err(err) => {
                tracing::warn!("BeforeOutbound hook blocked response: {}", err);
                return Ok(()); // Hook blocked, skip delivery
            }
            Ok(HookOutcome::Continue {
                modified: Some(new_content),
            }) => new_content,
            _ => response,
        };

        // Execute delivery
        let outgoing = OutgoingResponse::text(final_content);

        match &plan.mode {
            DeliveryMode::Reply { original } => {
                if let Err(e) = self.channels.respond(original, outgoing).await {
                    tracing::error!(
                        channel = %plan.channel,
                        error = %e,
                        "Failed to send reply to channel"
                    );
                }
            }
            DeliveryMode::Push => {
                if let Err(e) = self
                    .channels
                    .broadcast(&plan.channel, &plan.to, outgoing)
                    .await
                {
                    tracing::error!(
                        channel = %plan.channel,
                        to = %plan.to,
                        error = %e,
                        "Failed to push message to channel"
                    );
                }
            }
        }

        Ok(())
    }

    /// Deliver an error message to the user.
    ///
    /// Skips hooks — error messages should always reach the user.
    pub async fn deliver_error(
        &self,
        plan: &DeliveryPlan,
        error: &Error,
    ) {
        let outgoing = OutgoingResponse::text(format!("Error: {}", error));

        match &plan.mode {
            DeliveryMode::Reply { original } => {
                if let Err(e) = self.channels.respond(original, outgoing).await {
                    tracing::error!(
                        channel = %plan.channel,
                        send_error = %e,
                        original_error = %error,
                        "Failed to send error response to channel"
                    );
                }
            }
            DeliveryMode::Push => {
                if let Err(e) = self
                    .channels
                    .broadcast(&plan.channel, &plan.to, outgoing)
                    .await
                {
                    tracing::error!(
                        channel = %plan.channel,
                        to = %plan.to,
                        send_error = %e,
                        original_error = %error,
                        "Failed to push error message to channel"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn make_message(channel: &str, user_id: &str) -> IncomingMessage {
        IncomingMessage {
            id: Uuid::new_v4(),
            channel: channel.to_string(),
            user_id: user_id.to_string(),
            user_name: None,
            content: "test".to_string(),
            thread_id: None,
            received_at: Utc::now(),
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn test_plan_reply_basic() {
        let msg = make_message("wechat", "user123");
        let plan = OutboundRouter::plan_reply(&msg);

        assert_eq!(plan.channel, "wechat");
        assert_eq!(plan.to, "user123");
        assert!(plan.thread_id.is_none());
        assert!(matches!(plan.mode, DeliveryMode::Reply { .. }));
    }

    #[test]
    fn test_plan_reply_uses_signal_target() {
        let mut msg = make_message("signal", "uuid-123");
        msg.metadata = serde_json::json!({"signal_target": "+1234567890"});
        let plan = OutboundRouter::plan_reply(&msg);

        assert_eq!(plan.to, "+1234567890");
    }

    #[test]
    fn test_plan_reply_preserves_thread_id() {
        let mut msg = make_message("telegram", "user456");
        msg.thread_id = Some("thread-abc".to_string());
        let plan = OutboundRouter::plan_reply(&msg);

        assert_eq!(plan.thread_id.as_deref(), Some("thread-abc"));
    }

    #[test]
    fn test_plan_push_basic() {
        let plan = OutboundRouter::plan_push("wechat", "user789", None);

        assert_eq!(plan.channel, "wechat");
        assert_eq!(plan.to, "user789");
        assert!(plan.thread_id.is_none());
        assert!(matches!(plan.mode, DeliveryMode::Push));
    }

    #[test]
    fn test_plan_push_with_thread() {
        let plan = OutboundRouter::plan_push("telegram", "group-1", Some("topic-42"));

        assert_eq!(plan.channel, "telegram");
        assert_eq!(plan.to, "group-1");
        assert_eq!(plan.thread_id.as_deref(), Some("topic-42"));
    }
}

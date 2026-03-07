//! Agent message dispatcher.
//!
//! Routes incoming messages to the correct agent based on:
//! 1. Explicit `target_agent` in message metadata
//! 2. Agent binding rules (channel + peer matching)
//! 3. Default agent fallback
//!
//! Phase 2 deliverable: provides routing without modifying main.rs startup.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::agent::agent_loop::AgentInbox;
use crate::agent::registry::AgentRegistry;
use crate::channels::IncomingMessage;
use crate::db::{AgentBindingRecord, Database};

/// Routes incoming messages to the appropriate agent's inbox.
///
/// Thread-safe: bindings are cached behind `RwLock` and reloaded on demand.
/// All lookups go through the `AgentRegistry` which is also RwLock-protected.
pub struct AgentDispatcher {
    /// Agent registry for inbox lookup.
    registry: Arc<AgentRegistry>,

    /// Database for loading binding rules.
    store: Arc<dyn Database>,

    /// Cached binding rules, sorted by priority descending.
    bindings_cache: RwLock<Vec<AgentBindingRecord>>,
}

/// Result of a dispatch operation.
#[derive(Debug)]
pub struct DispatchResult {
    /// The resolved agent ID.
    pub agent_id: String,

    /// The agent's inbox sender.
    pub inbox: AgentInbox,

    /// How the agent was resolved.
    pub reason: DispatchReason,
}

/// Why a particular agent was selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchReason {
    /// Explicit `target_agent` in message metadata.
    Explicit,
    /// Matched an agent binding rule.
    Binding { binding_id: String },
    /// Fell back to the default agent.
    Default,
}

impl AgentDispatcher {
    /// Create a new dispatcher.
    pub fn new(registry: Arc<AgentRegistry>, store: Arc<dyn Database>) -> Self {
        Self {
            registry,
            store,
            bindings_cache: RwLock::new(Vec::new()),
        }
    }

    /// Reload binding rules from the database.
    ///
    /// Call this on startup and whenever bindings change.
    pub async fn refresh_bindings(&self) {
        match self.store.list_all_bindings().await {
            Ok(mut bindings) => {
                // Sort by priority descending (highest priority matched first)
                bindings.sort_by(|a, b| b.priority.cmp(&a.priority));
                // Filter to enabled bindings only
                bindings.retain(|b| b.enabled);
                let count = bindings.len();
                *self.bindings_cache.write().await = bindings;
                tracing::debug!("Refreshed {} agent binding(s)", count);
            }
            Err(e) => {
                tracing::error!("Failed to refresh agent bindings: {}", e);
            }
        }
    }

    /// Route an incoming message to the correct agent.
    ///
    /// Resolution order:
    /// 1. Explicit `target_agent` in message metadata
    /// 2. Best-matching agent binding rule
    /// 3. Default agent
    ///
    /// Returns `None` if no agent could be resolved (registry empty).
    pub async fn route(&self, message: &IncomingMessage) -> Option<DispatchResult> {
        // 1. Check explicit target_agent override
        if let Some(target) = message
            .metadata
            .get("target_agent")
            .and_then(|v| v.as_str())
        {
            if let Some(inbox) = self.registry.get(target).await {
                tracing::debug!(
                    target_agent = %target,
                    channel = %message.channel,
                    "Routed by explicit target_agent"
                );
                return Some(DispatchResult {
                    agent_id: target.to_string(),
                    inbox,
                    reason: DispatchReason::Explicit,
                });
            }
            // Explicit target not found or disabled — fall through to bindings
            tracing::warn!(
                target_agent = %target,
                "Explicit target_agent not found, falling back to binding match"
            );
        }

        // 2. Match against binding rules
        if let Some(result) = self.match_bindings(message).await {
            return Some(result);
        }

        // 3. Fall back to default agent
        let default_id = self.registry.default_agent_id().await;
        if let Some(inbox) = self.registry.get(&default_id).await {
            tracing::debug!(
                agent_id = %default_id,
                channel = %message.channel,
                "Routed to default agent"
            );
            return Some(DispatchResult {
                agent_id: default_id,
                inbox,
                reason: DispatchReason::Default,
            });
        }

        tracing::error!("No agent available for routing (registry empty?)");
        None
    }

    /// Match message against cached binding rules.
    ///
    /// Uses a scoring system: more specific matches score higher.
    /// Returns the best match (highest score, then highest priority).
    async fn match_bindings(&self, message: &IncomingMessage) -> Option<DispatchResult> {
        let bindings = self.bindings_cache.read().await;

        // Extract routing fields from metadata
        let peer_id = message
            .metadata
            .get("peer_id")
            .and_then(|v| v.as_str());
        let peer_type = message
            .metadata
            .get("peer_type")
            .and_then(|v| v.as_str());
        let account_id = message
            .metadata
            .get("account_id")
            .and_then(|v| v.as_str());

        let mut best_match: Option<(i32, &AgentBindingRecord)> = None;

        for binding in bindings.iter() {
            if let Some(score) = Self::score_binding(
                binding,
                &message.channel,
                account_id,
                peer_id,
                peer_type,
            ) {
                let current_best = best_match.as_ref().map(|(s, _)| *s).unwrap_or(-1);
                if score > current_best {
                    best_match = Some((score, binding));
                }
            }
        }

        if let Some((_score, binding)) = best_match {
            let agent_id = &binding.agent_id;
            if let Some(inbox) = self.registry.get(agent_id).await {
                tracing::debug!(
                    agent_id = %agent_id,
                    binding_id = %binding.id,
                    channel = %message.channel,
                    "Routed by binding match"
                );
                return Some(DispatchResult {
                    agent_id: agent_id.clone(),
                    inbox,
                    reason: DispatchReason::Binding {
                        binding_id: binding.id.to_string(),
                    },
                });
            }
        }

        None
    }

    /// Score a binding against a message's routing fields.
    ///
    /// Returns `None` if the binding doesn't match.
    /// Returns `Some(score)` where higher = more specific match.
    ///
    /// Scoring tiers:
    /// - Tier 4 (score 40+): channel + account + peer_id + peer_type
    /// - Tier 3 (score 30+): channel + account + peer_type
    /// - Tier 2 (score 20+): channel + peer_type
    /// - Tier 1 (score 10+): channel only
    /// - Tier 0 (score 1): wildcard (channel = "*")
    ///
    /// Within each tier, binding.priority is added as a tiebreaker.
    fn score_binding(
        binding: &AgentBindingRecord,
        channel: &str,
        account_id: Option<&str>,
        peer_id: Option<&str>,
        peer_type: Option<&str>,
    ) -> Option<i32> {
        // Channel match (required unless wildcard)
        let channel_match = match binding.channel.as_deref() {
            Some("*") | None => true, // Wildcard or unset = match any
            Some(c) => c == channel,
        };
        if !channel_match {
            return None;
        }

        // Account match
        let account_match = binding.account_id == "*"
            || account_id.is_some_and(|aid| aid == binding.account_id);

        // Peer ID match
        let peer_id_match = match &binding.peer_id {
            None => true,              // Binding doesn't restrict peer_id
            Some(pid) => peer_id.is_some_and(|p| p == pid),
        };

        // Peer type match
        let peer_type_match = match &binding.peer_type {
            None => true,              // Binding doesn't restrict peer_type
            Some(pt) => peer_type.is_some_and(|p| p == pt),
        };

        // All specified fields must match
        if !peer_id_match || !peer_type_match {
            return None;
        }

        // Calculate score based on specificity
        let mut score = 0;

        // Wildcard channel gets minimal score
        if binding.channel.as_deref() == Some("*") || binding.channel.is_none() {
            score += 1;
        } else {
            score += 10;
        }

        // Account specificity
        if account_match && binding.account_id != "*" {
            score += 10;
        }

        // Peer type specificity
        if binding.peer_type.is_some() && peer_type_match {
            score += 10;
        }

        // Peer ID specificity (most specific)
        if binding.peer_id.is_some() && peer_id_match {
            score += 10;
        }

        // Add binding priority as tiebreaker (capped to avoid tier jumping)
        let priority_bonus = binding.priority.clamp(0, 9);
        score += priority_bonus;

        Some(score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn make_binding(
        agent_id: &str,
        channel: Option<&str>,
        account_id: &str,
        peer_id: Option<&str>,
        peer_type: Option<&str>,
        priority: i32,
    ) -> AgentBindingRecord {
        AgentBindingRecord {
            id: Uuid::new_v4(),
            agent_id: agent_id.to_string(),
            channel: channel.map(|s| s.to_string()),
            account_id: account_id.to_string(),
            peer_id: peer_id.map(|s| s.to_string()),
            peer_type: peer_type.map(|s| s.to_string()),
            priority,
            enabled: true,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn test_wildcard_channel_matches_anything() {
        let binding = make_binding("newsbot", Some("*"), "*", None, None, 0);
        let score = AgentDispatcher::score_binding(&binding, "wechat", None, None, None);
        assert!(score.is_some());
        assert_eq!(score.unwrap(), 1); // Wildcard = tier 0
    }

    #[test]
    fn test_channel_match_scores_higher_than_wildcard() {
        let binding = make_binding("newsbot", Some("wechat"), "*", None, None, 0);
        let score = AgentDispatcher::score_binding(&binding, "wechat", None, None, None);
        assert!(score.is_some());
        assert!(score.unwrap() >= 10); // Tier 1
    }

    #[test]
    fn test_channel_mismatch_returns_none() {
        let binding = make_binding("newsbot", Some("telegram"), "*", None, None, 0);
        let score = AgentDispatcher::score_binding(&binding, "wechat", None, None, None);
        assert!(score.is_none());
    }

    #[test]
    fn test_peer_id_match_is_most_specific() {
        let binding = make_binding(
            "tutor",
            Some("wechat"),
            "*",
            Some("user123"),
            Some("dm"),
            0,
        );
        let score = AgentDispatcher::score_binding(
            &binding,
            "wechat",
            None,
            Some("user123"),
            Some("dm"),
        );
        assert!(score.is_some());
        assert!(score.unwrap() >= 30); // channel(10) + peer_type(10) + peer_id(10)
    }

    #[test]
    fn test_peer_id_mismatch_returns_none() {
        let binding = make_binding("tutor", Some("wechat"), "*", Some("user123"), None, 0);
        let score =
            AgentDispatcher::score_binding(&binding, "wechat", None, Some("user456"), None);
        assert!(score.is_none());
    }

    #[test]
    fn test_priority_as_tiebreaker() {
        let low = make_binding("newsbot", Some("wechat"), "*", None, None, 1);
        let high = make_binding("tutor", Some("wechat"), "*", None, None, 5);

        let low_score = AgentDispatcher::score_binding(&low, "wechat", None, None, None);
        let high_score = AgentDispatcher::score_binding(&high, "wechat", None, None, None);

        assert!(high_score.unwrap() > low_score.unwrap());
    }

    #[test]
    fn test_peer_type_match_adds_score() {
        let without = make_binding("main", Some("wechat"), "*", None, None, 0);
        let with = make_binding("main", Some("wechat"), "*", None, Some("dm"), 0);

        let score_without =
            AgentDispatcher::score_binding(&without, "wechat", None, None, Some("dm"));
        let score_with =
            AgentDispatcher::score_binding(&with, "wechat", None, None, Some("dm"));

        assert!(score_with.unwrap() > score_without.unwrap());
    }

    #[test]
    fn test_peer_type_mismatch_returns_none() {
        let binding = make_binding("tutor", Some("wechat"), "*", None, Some("group"), 0);
        let score =
            AgentDispatcher::score_binding(&binding, "wechat", None, None, Some("dm"));
        assert!(score.is_none());
    }

    #[test]
    fn test_account_match_adds_score() {
        let generic = make_binding("main", Some("wechat"), "*", None, None, 0);
        let specific =
            make_binding("main", Some("wechat"), "bot_account_1", None, None, 0);

        let generic_score =
            AgentDispatcher::score_binding(&generic, "wechat", Some("bot_account_1"), None, None);
        let specific_score =
            AgentDispatcher::score_binding(&specific, "wechat", Some("bot_account_1"), None, None);

        assert!(specific_score.unwrap() > generic_score.unwrap());
    }

    #[test]
    fn test_none_channel_matches_like_wildcard() {
        let binding = make_binding("main", None, "*", None, None, 0);
        let score = AgentDispatcher::score_binding(&binding, "telegram", None, None, None);
        assert!(score.is_some());
        assert_eq!(score.unwrap(), 1); // Same as wildcard
    }

    #[test]
    fn test_priority_capped_at_9() {
        let binding = make_binding("main", Some("wechat"), "*", None, None, 100);
        let score = AgentDispatcher::score_binding(&binding, "wechat", None, None, None);
        // channel(10) + priority(capped at 9) = 19
        assert_eq!(score.unwrap(), 19);
    }
}

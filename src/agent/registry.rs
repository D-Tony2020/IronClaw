//! Agent registry for multi-agent management.
//!
//! The `AgentRegistry` is the central authority for agent lifecycle:
//! - Creates agents from config definitions
//! - Reconciles config with DB state on startup
//! - Holds `Arc<Agent>` + `AgentInbox` per agent
//! - Provides lookup by agent_id
//!
//! Phase 1 delivers the registry + single-agent backward compatibility.
//! Phase 2 adds the dispatcher that routes messages to the correct agent.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::agent::agent_loop::{Agent, AgentDeps, AgentInbox};
use crate::agent::routine_engine::RoutineEngine;
use crate::agent::session_manager::SessionManager;
use crate::channels::ChannelManager;
use crate::config::agents::{AgentDefinition, AgentInstanceConfig};
use crate::config::{AgentConfig, HeartbeatConfig, RoutineConfig};
use crate::context::ContextManager;
use crate::db::{AgentRecord, Database};
use crate::error::{ConfigError, Error};
use crate::workspace::Workspace;

// ---------------------------------------------------------------------------
// AgentInstance — runtime wrapper for a live agent
// ---------------------------------------------------------------------------

/// A live agent instance with its inbox and metadata.
///
/// Each `AgentInstance` owns:
/// - The `Agent` behind an `Arc` (shared with the inbox loop task)
/// - An `AgentInbox` sender for dispatching messages
/// - Isolated `SessionManager` and `Workspace`
pub struct AgentInstance {
    /// Unique identifier (e.g. "main", "newsbot").
    pub agent_id: String,

    /// Database primary key (UUID) for workspace scoping and DB queries.
    pub db_id: Uuid,

    /// Per-agent configuration.
    pub config: AgentInstanceConfig,

    /// The agent, wrapped in Arc for sharing with the inbox loop.
    pub agent: Arc<Agent>,

    /// Sender for posting messages to this agent's inbox.
    pub inbox: AgentInbox,

    /// Per-agent session manager.
    pub session_manager: Arc<SessionManager>,

    /// Per-agent workspace.
    pub workspace: Option<Arc<Workspace>>,

    /// Per-agent routine engine (if enabled).
    pub routine_engine: Option<Arc<RoutineEngine>>,

    /// Handle to the inbox loop task (for cleanup).
    inbox_handle: Option<tokio::task::JoinHandle<()>>,
}

impl AgentInstance {
    /// Whether this agent is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

// ---------------------------------------------------------------------------
// AgentRegistry — manages all agent instances
// ---------------------------------------------------------------------------

/// Central registry for all agent instances.
///
/// Thread-safe: uses `RwLock` for concurrent reads (message dispatch)
/// with infrequent writes (hot-add/remove agents).
pub struct AgentRegistry {
    /// Agent instances keyed by agent_id.
    agents: RwLock<HashMap<String, AgentInstance>>,

    /// Database backend for agent persistence.
    store: Arc<dyn Database>,

    /// The default agent_id for unmatched messages.
    default_agent_id: RwLock<String>,
}

impl AgentRegistry {
    /// Create a new empty registry.
    pub fn new(store: Arc<dyn Database>) -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
            store,
            default_agent_id: RwLock::new("main".to_string()),
        }
    }

    /// Initialize the registry from config definitions.
    ///
    /// For each agent definition:
    /// 1. Reconcile with DB (insert if new, update if changed)
    /// 2. Create Agent instance with per-agent workspace + session manager
    /// 3. Start inbox loop
    ///
    /// This is the primary startup path, replacing direct `Agent::new()`.
    #[allow(clippy::too_many_arguments)]
    pub async fn initialize(
        &self,
        definitions: &[AgentDefinition],
        base_agent_config: &AgentConfig,
        base_deps: &AgentDeps,
        channels: &Arc<ChannelManager>,
        heartbeat_config: &Option<HeartbeatConfig>,
        hygiene_config: &Option<crate::config::HygieneConfig>,
        routine_config: &Option<RoutineConfig>,
    ) -> Result<(), Error> {
        // Reconcile config with DB
        self.reconcile_with_db(definitions).await?;

        // Find the default agent
        let default_id = definitions
            .iter()
            .find(|d| d.default)
            .map(|d| d.id.clone())
            .unwrap_or_else(|| "main".to_string());

        *self.default_agent_id.write().await = default_id.clone();

        // Create agent instances
        for def in definitions.iter().filter(|d| d.enabled) {
            let instance_config = def.to_instance_config();

            // Look up the DB record to get the UUID
            let db_record = self
                .store
                .get_agent(&def.id)
                .await
                .map_err(|e| Error::Config(ConfigError::ParseError(format!("Failed to get agent '{}' from DB: {}", def.id, e))))?
                .ok_or_else(|| {
                    Error::Config(ConfigError::ParseError(format!(
                        "Agent '{}' not found in DB after reconciliation",
                        def.id
                    )))
                })?;

            // Create per-agent workspace with agent UUID scoping
            let workspace = base_deps.workspace.as_ref().map(|base_ws| {
                // Create a new workspace scoped to this agent's UUID.
                // Uses the same storage backend but with agent_id isolation.
                if let Some(store) = &base_deps.store {
                    Arc::new(
                        Workspace::new_with_db("default", Arc::clone(store))
                            .with_agent(db_record.id),
                    )
                } else {
                    // Fallback: use the base workspace (no isolation)
                    Arc::clone(base_ws)
                }
            });

            // Create per-agent session manager
            let session_manager = Arc::new(SessionManager::new());

            // Create per-agent context manager
            let max_parallel = instance_config
                .max_parallel_jobs
                .unwrap_or(base_agent_config.max_parallel_jobs);
            let context_manager = Arc::new(ContextManager::new(max_parallel));

            // Clone deps and apply per-agent overrides
            let mut agent_deps = base_deps.clone();
            if let Some(ref ws) = workspace {
                agent_deps.workspace = Some(Arc::clone(ws));
            }

            // Build per-agent config (merge instance overrides into base)
            let mut agent_config = base_agent_config.clone();
            agent_config.name = def.id.clone();
            if let Some(max_jobs) = instance_config.max_parallel_jobs {
                agent_config.max_parallel_jobs = max_jobs;
            }

            // Determine heartbeat config for this agent
            let agent_heartbeat = instance_config
                .heartbeat
                .clone()
                .or_else(|| heartbeat_config.clone());

            // Create the Agent
            let agent = Agent::new(
                agent_config,
                agent_deps,
                Arc::clone(channels),
                agent_heartbeat,
                hygiene_config.clone(),
                routine_config.clone(),
                Some(context_manager),
                Some(session_manager.clone()),
            );

            let agent = Arc::new(agent);

            // Create inbox
            let (inbox_tx, inbox_rx) = Agent::create_inbox();

            // Spawn inbox loop
            let loop_agent = agent.clone();
            let inbox_handle = tokio::spawn(async move {
                loop_agent.start_inbox_loop(inbox_rx, None).await;
            });

            let instance = AgentInstance {
                agent_id: def.id.clone(),
                db_id: db_record.id,
                config: instance_config,
                agent,
                inbox: inbox_tx,
                session_manager,
                workspace,
                routine_engine: None,
                inbox_handle: Some(inbox_handle),
            };

            self.agents.write().await.insert(def.id.clone(), instance);
            tracing::info!(agent_id = %def.id, db_id = %db_record.id, "Registered agent");
        }

        let count = self.agents.read().await.len();
        tracing::info!(
            "AgentRegistry initialized: {} agent(s), default='{}'",
            count,
            default_id
        );

        Ok(())
    }

    /// Reconcile config definitions with the database.
    ///
    /// - New agents in config → insert into DB
    /// - Existing agents → update display_name, is_default, enabled
    /// - Agents in DB but not in config → disable (soft delete)
    async fn reconcile_with_db(&self, definitions: &[AgentDefinition]) -> Result<(), Error> {
        let existing = self
            .store
            .list_agents()
            .await
            .map_err(|e| Error::Config(ConfigError::ParseError(format!("Failed to list agents from DB: {}", e))))?;

        let existing_ids: HashMap<String, AgentRecord> = existing
            .into_iter()
            .map(|r| (r.agent_id.clone(), r))
            .collect();

        let config_ids: std::collections::HashSet<String> =
            definitions.iter().map(|d| d.id.clone()).collect();

        // Insert or update agents from config
        for def in definitions {
            if let Some(existing_record) = existing_ids.get(&def.id) {
                // Update existing record if needed
                let needs_update = existing_record.display_name.as_deref()
                    != def.display_name.as_deref()
                    || existing_record.is_default != def.default
                    || existing_record.enabled != def.enabled;

                if needs_update {
                    let mut updated = existing_record.clone();
                    updated.display_name = def.display_name.clone();
                    updated.is_default = def.default;
                    updated.enabled = def.enabled;
                    updated.updated_at = Utc::now();

                    self.store.update_agent(&updated).await.map_err(|e| {
                        Error::Config(ConfigError::ParseError(format!("Failed to update agent '{}': {}", def.id, e)))
                    })?;

                    tracing::debug!(agent_id = %def.id, "Reconciled agent (updated)");
                }
            } else {
                // Insert new agent
                let config_json =
                    serde_json::to_string(&def.to_instance_config()).unwrap_or_default();

                let record = AgentRecord {
                    id: Uuid::new_v4(),
                    agent_id: def.id.clone(),
                    display_name: def.display_name.clone(),
                    description: None,
                    is_default: def.default,
                    enabled: def.enabled,
                    config_json,
                    workspace_prefix: Some(format!("agents/{}/", def.id)),
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };

                self.store.create_agent(&record).await.map_err(|e| {
                    Error::Config(ConfigError::ParseError(format!("Failed to create agent '{}': {}", def.id, e)))
                })?;

                tracing::info!(agent_id = %def.id, "Reconciled agent (created)");
            }
        }

        // Disable agents that are in DB but not in config
        for (agent_id, record) in &existing_ids {
            if !config_ids.contains(agent_id) && record.enabled {
                let mut disabled = record.clone();
                disabled.enabled = false;
                disabled.updated_at = Utc::now();

                self.store.update_agent(&disabled).await.map_err(|e| {
                    Error::Config(ConfigError::ParseError(format!("Failed to disable agent '{}': {}", agent_id, e)))
                })?;

                tracing::info!(
                    agent_id = %agent_id,
                    "Disabled agent (removed from config)"
                );
            }
        }

        Ok(())
    }

    // -------------------------------------------------------------------
    // Lookup API
    // -------------------------------------------------------------------

    /// Get an agent instance by ID.
    pub async fn get(&self, agent_id: &str) -> Option<AgentInbox> {
        self.agents
            .read()
            .await
            .get(agent_id)
            .filter(|inst| inst.is_enabled())
            .map(|inst| inst.inbox.clone())
    }

    /// Get the default agent's inbox.
    pub async fn get_default(&self) -> Option<AgentInbox> {
        let default_id = self.default_agent_id.read().await.clone();
        self.get(&default_id).await
    }

    /// Get the default agent's ID.
    pub async fn default_agent_id(&self) -> String {
        self.default_agent_id.read().await.clone()
    }

    /// Get an agent instance (full struct access, for internal use).
    pub async fn get_instance(&self, agent_id: &str) -> Option<Arc<Agent>> {
        self.agents
            .read()
            .await
            .get(agent_id)
            .map(|inst| inst.agent.clone())
    }

    /// List all registered agent IDs.
    pub async fn list_ids(&self) -> Vec<String> {
        self.agents.read().await.keys().cloned().collect()
    }

    /// List all agent IDs with their enabled status and default flag.
    ///
    /// Returns `(agent_id, enabled, is_default)` tuples.
    pub async fn list_agents_info(&self) -> Vec<(String, bool, bool)> {
        let default_id = self.default_agent_id.read().await.clone();
        self.agents
            .read()
            .await
            .values()
            .map(|inst| {
                let is_default = inst.agent_id == default_id;
                (inst.agent_id.clone(), inst.config.enabled, is_default)
            })
            .collect()
    }

    /// Number of registered agents.
    pub async fn count(&self) -> usize {
        self.agents.read().await.len()
    }

    // -------------------------------------------------------------------
    // Lifecycle
    // -------------------------------------------------------------------

    /// Shutdown all agents gracefully.
    ///
    /// Drops inbox senders (triggering loop exit) and aborts loop tasks.
    pub async fn shutdown_all(&self) {
        let mut agents = self.agents.write().await;
        for (id, instance) in agents.iter_mut() {
            // Drop the inbox sender would happen when instance is dropped,
            // but we explicitly abort the task for faster cleanup.
            if let Some(handle) = instance.inbox_handle.take() {
                handle.abort();
            }
            tracing::debug!(agent_id = %id, "Shut down agent");
        }
        agents.clear();
        tracing::info!("All agents shut down");
    }

    /// Get the default agent for backward-compatible single-agent mode.
    ///
    /// Returns the `Arc<Agent>` so the caller can invoke `run()` on it
    /// (which starts its own bridge loop). This is a migration path:
    /// Phase 2 replaces this with the dispatcher.
    pub async fn take_default_agent(&self) -> Option<Arc<Agent>> {
        let default_id = self.default_agent_id.read().await.clone();
        let agents = self.agents.read().await;
        agents.get(&default_id).map(|inst| inst.agent.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_instance_config_default() {
        let config = AgentInstanceConfig::default();
        assert!(config.enabled);
        assert_eq!(config.display_name, "Agent");
        assert!(config.model_override.is_none());
        assert!(config.enabled_skills.is_none());
        assert_eq!(config.dm_scope, crate::config::agents::DmScope::Main);
    }

    #[test]
    fn test_agent_definition_to_instance_config() {
        let def = AgentDefinition {
            id: "newsbot".to_string(),
            display_name: Some("News Bot".to_string()),
            default: false,
            model: Some("gemini-2.5-flash".to_string()),
            fallback_model: None,
            skills: Some(vec!["news-curator".to_string()]),
            tools: None,
            workspace_dir: None,
            heartbeat: None,
            dm_scope: Some(crate::config::agents::DmScope::PerPeer),
            system_prompt_extra: Some("You are a news bot.".to_string()),
            max_parallel_jobs: Some(2),
            enabled: true,
        };

        let config = def.to_instance_config();
        assert_eq!(config.display_name, "News Bot");
        assert_eq!(
            config.model_override.as_deref(),
            Some("gemini-2.5-flash")
        );
        assert_eq!(
            config.enabled_skills,
            Some(vec!["news-curator".to_string()])
        );
        assert_eq!(
            config.dm_scope,
            crate::config::agents::DmScope::PerPeer
        );
        assert_eq!(config.max_parallel_jobs, Some(2));
        assert_eq!(
            config.system_prompt_extra.as_deref(),
            Some("You are a news bot.")
        );
    }

    #[test]
    fn test_agent_definition_defaults_display_name_to_id() {
        let def = AgentDefinition {
            id: "tutor".to_string(),
            display_name: None,
            default: false,
            model: None,
            fallback_model: None,
            skills: None,
            tools: None,
            workspace_dir: None,
            heartbeat: None,
            dm_scope: None,
            system_prompt_extra: None,
            max_parallel_jobs: None,
            enabled: true,
        };

        let config = def.to_instance_config();
        assert_eq!(config.display_name, "tutor");
    }
}

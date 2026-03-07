//! Multi-agent configuration.
//!
//! Defines per-agent overrides on top of the global `AgentConfig`.
//! Loaded from TOML `[[agents.list]]` or environment variables.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::HeartbeatConfig;

// ---------------------------------------------------------------------------
// DmScope — session isolation strategy per agent
// ---------------------------------------------------------------------------

/// Controls how sessions are scoped for an agent.
///
/// - `Main`: All DMs share one session (default, suitable for single-user setups).
/// - `PerPeer`: Each user has an isolated session (multi-tenant).
/// - `PerChannelPeer`: Each (channel, user) pair has an isolated session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DmScope {
    #[default]
    Main,
    PerPeer,
    PerChannelPeer,
}

// ---------------------------------------------------------------------------
// AgentInstanceConfig — per-agent overrides
// ---------------------------------------------------------------------------

/// Per-agent configuration that extends the global [`AgentConfig`] defaults.
///
/// Fields that are `None` inherit the global default. This allows each agent
/// to selectively override model, skills, system prompt, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInstanceConfig {
    /// Human-readable name (e.g. "Tech News Bot").
    pub display_name: String,

    /// Whether this agent is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Override the LLM model for this agent (e.g. "gemini-2.5-flash").
    pub model_override: Option<String>,

    /// Fallback model if the primary model fails.
    pub fallback_model: Option<String>,

    /// Skill allowlist. `None` = all skills available.
    pub enabled_skills: Option<Vec<String>>,

    /// Tool allowlist. `None` = all tools available.
    pub enabled_tools: Option<Vec<String>>,

    /// Extra text appended to the system prompt for this agent.
    pub system_prompt_extra: Option<String>,

    /// Per-agent heartbeat config override.
    pub heartbeat: Option<HeartbeatConfig>,

    /// Override max parallel jobs.
    pub max_parallel_jobs: Option<usize>,

    /// Session isolation strategy.
    #[serde(default)]
    pub dm_scope: DmScope,
}

impl Default for AgentInstanceConfig {
    fn default() -> Self {
        Self {
            display_name: "Agent".to_string(),
            enabled: true,
            model_override: None,
            fallback_model: None,
            enabled_skills: None,
            enabled_tools: None,
            system_prompt_extra: None,
            heartbeat: None,
            max_parallel_jobs: None,
            dm_scope: DmScope::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// AgentDefinition — TOML-level agent declaration
// ---------------------------------------------------------------------------

/// A single agent definition from the config file.
///
/// This maps to `[[agents.list]]` in `config.toml`:
/// ```toml
/// [[agents.list]]
/// id = "newsbot"
/// display_name = "Tech News Bot"
/// default = false
/// model = "gemini-2.5-flash"
/// skills = ["news-curator"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// Unique agent identifier (e.g. "main", "newsbot", "tutor").
    pub id: String,

    /// Human-readable display name.
    pub display_name: Option<String>,

    /// Whether this is the default agent for unmatched messages.
    #[serde(default)]
    pub default: bool,

    /// LLM model override.
    pub model: Option<String>,

    /// Fallback model.
    pub fallback_model: Option<String>,

    /// Skill allowlist.
    pub skills: Option<Vec<String>>,

    /// Tool allowlist.
    pub tools: Option<Vec<String>>,

    /// Custom workspace directory (relative to ~/.ironclaw/).
    pub workspace_dir: Option<PathBuf>,

    /// Heartbeat override.
    pub heartbeat: Option<HeartbeatConfig>,

    /// Session isolation strategy.
    pub dm_scope: Option<DmScope>,

    /// Extra system prompt.
    pub system_prompt_extra: Option<String>,

    /// Max parallel jobs override.
    pub max_parallel_jobs: Option<usize>,

    /// Whether this agent is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl AgentDefinition {
    /// Convert to runtime config, applying defaults.
    pub fn to_instance_config(&self) -> AgentInstanceConfig {
        AgentInstanceConfig {
            display_name: self
                .display_name
                .clone()
                .unwrap_or_else(|| self.id.clone()),
            enabled: self.enabled,
            model_override: self.model.clone(),
            fallback_model: self.fallback_model.clone(),
            enabled_skills: self.skills.clone(),
            enabled_tools: self.tools.clone(),
            system_prompt_extra: self.system_prompt_extra.clone(),
            heartbeat: self.heartbeat.clone(),
            max_parallel_jobs: self.max_parallel_jobs,
            dm_scope: self.dm_scope.clone().unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// AgentsConfig — top-level multi-agent section
// ---------------------------------------------------------------------------

/// Multi-agent configuration section.
///
/// When absent from config, defaults to a single "main" agent that inherits
/// all global settings. This preserves backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsConfig {
    /// Agent definitions.
    #[serde(default = "default_agent_list")]
    pub list: Vec<AgentDefinition>,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            list: default_agent_list(),
        }
    }
}

/// Default: single "main" agent with default settings.
fn default_agent_list() -> Vec<AgentDefinition> {
    vec![AgentDefinition {
        id: "main".to_string(),
        display_name: Some("Main Agent".to_string()),
        default: true,
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
    }]
}

fn default_true() -> bool {
    true
}

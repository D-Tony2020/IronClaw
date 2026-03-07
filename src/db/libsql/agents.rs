//! AgentStore implementation for the libSQL backend.
//!
//! Iron-OpenClaw Phase 0: CRUD for agents and agent_bindings tables.

use async_trait::async_trait;
use super::{LibSqlBackend, fmt_ts, get_i64, get_opt_text, get_text, get_ts, opt_text_owned};
use crate::db::{AgentBindingRecord, AgentRecord, AgentStore};
use crate::error::DatabaseError;

#[async_trait]
impl AgentStore for LibSqlBackend {
    async fn create_agent(&self, agent: &AgentRecord) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        conn.execute(
            "INSERT INTO agents (id, agent_id, display_name, description, is_default, enabled, config_json, workspace_prefix, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            libsql::params![
                agent.id.to_string(),
                agent.agent_id.clone(),
                opt_text_owned(agent.display_name.clone()),
                opt_text_owned(agent.description.clone()),
                agent.is_default as i64,
                agent.enabled as i64,
                agent.config_json.clone(),
                opt_text_owned(agent.workspace_prefix.clone()),
                fmt_ts(&agent.created_at),
                fmt_ts(&agent.updated_at),
            ],
        )
        .await
        .map_err(|e| DatabaseError::Query(format!("create_agent: {e}")))?;
        Ok(())
    }

    async fn get_agent(&self, agent_id: &str) -> Result<Option<AgentRecord>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, agent_id, display_name, description, is_default, enabled, config_json, workspace_prefix, created_at, updated_at
                 FROM agents WHERE agent_id = ?1",
                libsql::params![agent_id],
            )
            .await
            .map_err(|e| DatabaseError::Query(format!("get_agent: {e}")))?;

        match rows.next().await {
            Ok(Some(row)) => Ok(Some(row_to_agent(&row))),
            Ok(None) => Ok(None),
            Err(e) => Err(DatabaseError::Query(format!("get_agent row: {e}"))),
        }
    }

    async fn list_agents(&self) -> Result<Vec<AgentRecord>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, agent_id, display_name, description, is_default, enabled, config_json, workspace_prefix, created_at, updated_at
                 FROM agents ORDER BY is_default DESC, agent_id ASC",
                libsql::params![],
            )
            .await
            .map_err(|e| DatabaseError::Query(format!("list_agents: {e}")))?;

        let mut agents = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            agents.push(row_to_agent(&row));
        }
        Ok(agents)
    }

    async fn update_agent(&self, agent: &AgentRecord) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        let affected = conn
            .execute(
                "UPDATE agents SET display_name = ?1, description = ?2, is_default = ?3, enabled = ?4, config_json = ?5, workspace_prefix = ?6, updated_at = ?7
                 WHERE agent_id = ?8",
                libsql::params![
                    opt_text_owned(agent.display_name.clone()),
                    opt_text_owned(agent.description.clone()),
                    agent.is_default as i64,
                    agent.enabled as i64,
                    agent.config_json.clone(),
                    opt_text_owned(agent.workspace_prefix.clone()),
                    fmt_ts(&chrono::Utc::now()),
                    agent.agent_id.clone(),
                ],
            )
            .await
            .map_err(|e| DatabaseError::Query(format!("update_agent: {e}")))?;
        if affected == 0 {
            return Err(DatabaseError::NotFound {
                entity: "agent".to_string(),
                id: agent.agent_id.clone(),
            });
        }
        Ok(())
    }

    async fn delete_agent(&self, agent_id: &str) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        conn.execute(
            "DELETE FROM agents WHERE agent_id = ?1",
            libsql::params![agent_id],
        )
        .await
        .map_err(|e| DatabaseError::Query(format!("delete_agent: {e}")))?;
        Ok(())
    }

    async fn get_default_agent(&self) -> Result<Option<AgentRecord>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, agent_id, display_name, description, is_default, enabled, config_json, workspace_prefix, created_at, updated_at
                 FROM agents WHERE is_default = 1 AND enabled = 1 LIMIT 1",
                libsql::params![],
            )
            .await
            .map_err(|e| DatabaseError::Query(format!("get_default_agent: {e}")))?;

        match rows.next().await {
            Ok(Some(row)) => Ok(Some(row_to_agent(&row))),
            Ok(None) => Ok(None),
            Err(e) => Err(DatabaseError::Query(format!("get_default_agent row: {e}"))),
        }
    }

    async fn create_binding(&self, binding: &AgentBindingRecord) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        conn.execute(
            "INSERT INTO agent_bindings (id, agent_id, channel, account_id, peer_id, peer_type, priority, enabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            libsql::params![
                binding.id.to_string(),
                binding.agent_id.clone(),
                opt_text_owned(binding.channel.clone()),
                binding.account_id.clone(),
                opt_text_owned(binding.peer_id.clone()),
                opt_text_owned(binding.peer_type.clone()),
                binding.priority as i64,
                binding.enabled as i64,
                fmt_ts(&binding.created_at),
            ],
        )
        .await
        .map_err(|e| DatabaseError::Query(format!("create_binding: {e}")))?;
        Ok(())
    }

    async fn list_bindings(
        &self,
        agent_id: &str,
    ) -> Result<Vec<AgentBindingRecord>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, agent_id, channel, account_id, peer_id, peer_type, priority, enabled, created_at
                 FROM agent_bindings WHERE agent_id = ?1 ORDER BY priority DESC",
                libsql::params![agent_id],
            )
            .await
            .map_err(|e| DatabaseError::Query(format!("list_bindings: {e}")))?;

        let mut bindings = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            bindings.push(row_to_binding(&row));
        }
        Ok(bindings)
    }

    async fn list_all_bindings(&self) -> Result<Vec<AgentBindingRecord>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, agent_id, channel, account_id, peer_id, peer_type, priority, enabled, created_at
                 FROM agent_bindings WHERE enabled = 1 ORDER BY priority DESC",
                libsql::params![],
            )
            .await
            .map_err(|e| DatabaseError::Query(format!("list_all_bindings: {e}")))?;

        let mut bindings = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            bindings.push(row_to_binding(&row));
        }
        Ok(bindings)
    }

    async fn delete_binding(&self, id: &str) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        conn.execute(
            "DELETE FROM agent_bindings WHERE id = ?1",
            libsql::params![id],
        )
        .await
        .map_err(|e| DatabaseError::Query(format!("delete_binding: {e}")))?;
        Ok(())
    }
}

/// Convert a libSQL row to an AgentRecord.
fn row_to_agent(row: &libsql::Row) -> AgentRecord {
    AgentRecord {
        id: get_text(row, 0).parse().unwrap_or_default(),
        agent_id: get_text(row, 1),
        display_name: get_opt_text(row, 2),
        description: get_opt_text(row, 3),
        is_default: get_i64(row, 4) != 0,
        enabled: get_i64(row, 5) != 0,
        config_json: get_text(row, 6),
        workspace_prefix: get_opt_text(row, 7),
        created_at: get_ts(row, 8),
        updated_at: get_ts(row, 9),
    }
}

/// Convert a libSQL row to an AgentBindingRecord.
fn row_to_binding(row: &libsql::Row) -> AgentBindingRecord {
    AgentBindingRecord {
        id: get_text(row, 0).parse().unwrap_or_default(),
        agent_id: get_text(row, 1),
        channel: get_opt_text(row, 2),
        account_id: row
            .get::<String>(3)
            .unwrap_or_else(|_| "*".to_string()),
        peer_id: get_opt_text(row, 4),
        peer_type: get_opt_text(row, 5),
        priority: get_i64(row, 6) as i32,
        enabled: get_i64(row, 7) != 0,
        created_at: get_ts(row, 8),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use crate::db::libsql::LibSqlBackend;
    use crate::db::{AgentBindingRecord, AgentRecord, AgentStore, Database};

    /// Helper: create a file-backed temp DB with migrations applied.
    ///
    /// Uses a temp file rather than `:memory:` because libsql in-memory
    /// databases don't share state across connections (each `connect()`
    /// sees an independent database).
    async fn setup() -> (LibSqlBackend, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_agents.db");
        let backend = LibSqlBackend::new_local(&db_path).await.unwrap();
        backend.run_migrations().await.unwrap();
        (backend, dir) // dir must outlive backend to keep the temp directory
    }

    fn make_agent(agent_id: &str, is_default: bool) -> AgentRecord {
        let now = Utc::now();
        AgentRecord {
            id: Uuid::new_v4(),
            agent_id: agent_id.to_string(),
            display_name: Some(format!("{} Agent", agent_id)),
            description: Some(format!("Test agent {}", agent_id)),
            is_default,
            enabled: true,
            config_json: "{}".to_string(),
            workspace_prefix: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn test_create_and_get_agent() {
        let (db, _dir) = setup().await;
        let agent = make_agent("test-agent", false);
        db.create_agent(&agent).await.unwrap();

        let fetched = db.get_agent("test-agent").await.unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.agent_id, "test-agent");
        assert_eq!(fetched.display_name.as_deref(), Some("test-agent Agent"));
        assert!(!fetched.is_default);
        assert!(fetched.enabled);
    }

    #[tokio::test]
    async fn test_get_nonexistent_agent() {
        let (db, _dir) = setup().await;
        let fetched = db.get_agent("nonexistent").await.unwrap();
        assert!(fetched.is_none());
    }

    #[tokio::test]
    async fn test_list_agents() {
        let (db, _dir) = setup().await;
        db.create_agent(&make_agent("alpha", false)).await.unwrap();
        db.create_agent(&make_agent("beta", true)).await.unwrap();
        db.create_agent(&make_agent("gamma", false)).await.unwrap();

        let agents = db.list_agents().await.unwrap();
        assert_eq!(agents.len(), 3);
        // Default agent should come first (ORDER BY is_default DESC)
        assert_eq!(agents[0].agent_id, "beta");
    }

    #[tokio::test]
    async fn test_update_agent() {
        let (db, _dir) = setup().await;
        let mut agent = make_agent("updatable", false);
        db.create_agent(&agent).await.unwrap();

        agent.display_name = Some("Updated Name".to_string());
        agent.enabled = false;
        db.update_agent(&agent).await.unwrap();

        let fetched = db.get_agent("updatable").await.unwrap().unwrap();
        assert_eq!(fetched.display_name.as_deref(), Some("Updated Name"));
        assert!(!fetched.enabled);
    }

    #[tokio::test]
    async fn test_update_nonexistent_agent_returns_not_found() {
        let (db, _dir) = setup().await;
        let agent = make_agent("ghost", false);
        let result = db.update_agent(&agent).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_delete_agent() {
        let (db, _dir) = setup().await;
        db.create_agent(&make_agent("deletable", false))
            .await
            .unwrap();
        db.delete_agent("deletable").await.unwrap();

        let fetched = db.get_agent("deletable").await.unwrap();
        assert!(fetched.is_none());
    }

    #[tokio::test]
    async fn test_get_default_agent() {
        let (db, _dir) = setup().await;
        db.create_agent(&make_agent("non-default", false))
            .await
            .unwrap();
        db.create_agent(&make_agent("the-default", true))
            .await
            .unwrap();

        let default = db.get_default_agent().await.unwrap();
        assert!(default.is_some());
        assert_eq!(default.unwrap().agent_id, "the-default");
    }

    #[tokio::test]
    async fn test_get_default_agent_none_when_disabled() {
        let (db, _dir) = setup().await;
        let mut agent = make_agent("disabled-default", true);
        agent.enabled = false;
        db.create_agent(&agent).await.unwrap();

        let default = db.get_default_agent().await.unwrap();
        assert!(default.is_none());
    }

    #[tokio::test]
    async fn test_create_and_list_bindings() {
        let (db, _dir) = setup().await;
        db.create_agent(&make_agent("bound-agent", false))
            .await
            .unwrap();

        let binding = AgentBindingRecord {
            id: Uuid::new_v4(),
            agent_id: "bound-agent".to_string(),
            channel: Some("wechat".to_string()),
            account_id: "*".to_string(),
            peer_id: Some("user123".to_string()),
            peer_type: Some("dm".to_string()),
            priority: 10,
            enabled: true,
            created_at: Utc::now(),
        };
        db.create_binding(&binding).await.unwrap();

        let bindings = db.list_bindings("bound-agent").await.unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].channel.as_deref(), Some("wechat"));
        assert_eq!(bindings[0].peer_id.as_deref(), Some("user123"));
        assert_eq!(bindings[0].priority, 10);
    }

    #[tokio::test]
    async fn test_list_all_bindings_excludes_disabled() {
        let (db, _dir) = setup().await;
        db.create_agent(&make_agent("agent-a", false))
            .await
            .unwrap();

        let enabled = AgentBindingRecord {
            id: Uuid::new_v4(),
            agent_id: "agent-a".to_string(),
            channel: Some("telegram".to_string()),
            account_id: "*".to_string(),
            peer_id: None,
            peer_type: None,
            priority: 5,
            enabled: true,
            created_at: Utc::now(),
        };
        let disabled = AgentBindingRecord {
            id: Uuid::new_v4(),
            agent_id: "agent-a".to_string(),
            channel: Some("wechat".to_string()),
            account_id: "*".to_string(),
            peer_id: None,
            peer_type: None,
            priority: 1,
            enabled: false,
            created_at: Utc::now(),
        };
        db.create_binding(&enabled).await.unwrap();
        db.create_binding(&disabled).await.unwrap();

        let all = db.list_all_bindings().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].channel.as_deref(), Some("telegram"));
    }

    #[tokio::test]
    async fn test_delete_binding() {
        let (db, _dir) = setup().await;
        db.create_agent(&make_agent("agent-del", false))
            .await
            .unwrap();

        let binding = AgentBindingRecord {
            id: Uuid::new_v4(),
            agent_id: "agent-del".to_string(),
            channel: None,
            account_id: "*".to_string(),
            peer_id: None,
            peer_type: None,
            priority: 0,
            enabled: true,
            created_at: Utc::now(),
        };
        let bid = binding.id.to_string();
        db.create_binding(&binding).await.unwrap();

        db.delete_binding(&bid).await.unwrap();
        let bindings = db.list_bindings("agent-del").await.unwrap();
        assert!(bindings.is_empty());
    }

    #[tokio::test]
    async fn test_agent_id_uniqueness() {
        let (db, _dir) = setup().await;
        db.create_agent(&make_agent("unique-id", false))
            .await
            .unwrap();
        let result = db.create_agent(&make_agent("unique-id", false)).await;
        assert!(result.is_err(), "duplicate agent_id should fail");
    }

    #[tokio::test]
    async fn test_routines_table_has_agent_id_column() {
        let (db, _dir) = setup().await;
        let conn = db.connect().await.unwrap();

        // Verify the routines table has an agent_id column
        let mut rows = conn
            .query("PRAGMA table_info(routines)", ())
            .await
            .unwrap();

        let mut found_agent_id = false;
        while let Ok(Some(row)) = rows.next().await {
            let name: String = row.get(1).unwrap();
            if name == "agent_id" {
                found_agent_id = true;
                break;
            }
        }
        assert!(found_agent_id, "routines table should have agent_id column");
    }

    #[tokio::test]
    async fn test_migration_v10_tables_exist() {
        let (backend, _dir) = setup().await;
        let conn = backend.connect().await.unwrap();

        // Check which migrations have been applied
        let mut rows = conn
            .query("SELECT version, name FROM _migrations ORDER BY version", ())
            .await
            .unwrap();
        let mut versions = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            let v: i64 = row.get(0).unwrap();
            let n: String = row.get(1).unwrap();
            versions.push((v, n));
        }
        assert!(
            versions.iter().any(|(v, _)| *v == 10),
            "V10 migration should be applied. Applied: {:?}",
            versions,
        );

        // Verify agents table exists
        let mut rows = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='agents'",
                (),
            )
            .await
            .unwrap();
        assert!(rows.next().await.unwrap().is_some(), "agents table should exist");

        // Verify agent_bindings table exists
        let mut rows = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='agent_bindings'",
                (),
            )
            .await
            .unwrap();
        assert!(
            rows.next().await.unwrap().is_some(),
            "agent_bindings table should exist"
        );
    }
}
